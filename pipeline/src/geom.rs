//! Rings, polygons and WKB.
//!
//! This is the part libosmium would do for us. Two jobs:
//!
//!   * **assemble** — a multipolygon relation is a bag of ways with no promised
//!     order, direction or correctness. They have to be chained end-to-end into
//!     closed rings before anything geometric can be said about them.
//!   * **classify** — which ring is a hole in which. OSM `outer`/`inner` roles
//!     are advisory and frequently wrong, so nesting is decided by containment,
//!     the same way libosmium decides it.
//!
//! Everything here works on WGS84 degrees, because that is what the PBF stores
//! and what DuckDB is handed. The only Mercator arithmetic is the size filter,
//! which has to happen in projected units to mean anything on screen.

/// Half the width of the Web Mercator plane, in projected metres.
pub const WORLD: f64 = 20037508.342789244;

pub type Pt = [f64; 2];

/// Web Mercator northing. Clamped to the square plane the tile grid covers.
pub fn mercator_y(lat: f64) -> f64 {
    let lat = lat.clamp(-85.05, 85.05);
    ((90.0 + lat) * std::f64::consts::PI / 360.0).tan().ln() * WORLD / std::f64::consts::PI
}

/// A closed ring, with the two things every later step asks for cached.
pub struct Ring {
    pub pts: Vec<Pt>,
    /// [min_x, min_y, max_x, max_y] in degrees.
    pub bbox: [f64; 4],
    /// Shoelace area, signed: positive is counter-clockwise.
    pub area: f64,
}

impl Ring {
    pub fn new(pts: Vec<Pt>) -> Ring {
        let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut acc = 0.0;
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            bbox[0] = bbox[0].min(a[0]);
            bbox[1] = bbox[1].min(a[1]);
            bbox[2] = bbox[2].max(a[0]);
            bbox[3] = bbox[3].max(a[1]);
            acc += a[0] * b[1] - b[0] * a[1];
        }
        Ring { pts, bbox, area: acc / 2.0 }
    }

    /// Reverse in place unless the winding already matches `ccw`.
    fn orient(&mut self, ccw: bool) {
        if (self.area > 0.0) != ccw {
            self.pts.reverse();
            self.area = -self.area;
        }
    }
}

/// One outer ring followed by its holes.
pub type Polygon = Vec<Ring>;

// --- assembly -------------------------------------------------------------

/// Turn the member ways of a multipolygon into closed rings.
///
/// This works on *segments*, not on whole ways, and that is the whole trick.
/// A relation's members are not its rings: two members that run along the same
/// wall describe one shape with the wall inside it, and the wall has to
/// disappear. Cancelling every segment that appears twice is what does that —
/// nine adjacent building outlines in one relation are nine rings before this
/// step and one outline after. Skipping it is not a rounding error: the shape
/// comes out shattered into its members, which is how this was first written
/// and how 25 of Picardie's 5,160 multipolygons came out wrong.
///
/// Segments are keyed on packed *locations*, not on node ids, which is the
/// other thing libosmium does and the reason it is copied here. Keying on ids
/// looks more principled — two ways meet because they share a node — but it
/// takes OSM at its word. Two nodes at the same coordinate are a common enough
/// data error that a relation whose only member is "closed" that way exists in
/// Picardie; on locations it closes, on ids it is garbage.
///
/// Returns `None` if what is left cannot be closed — libosmium reports that as
/// a broken area and emits nothing, and so do we, rather than shipping a
/// plausible-looking wrong shape.
pub fn assemble_rings(parts: Vec<Vec<u64>>) -> Option<Vec<Vec<u64>>> {
    use std::collections::HashMap;

    // Undirected, so a wall walked in opposite directions by its two owners
    // still matches.
    let mut segments: Vec<[u64; 2]> = Vec::new();
    for part in &parts {
        for w in part.windows(2) {
            if w[0] != w[1] {
                segments.push(if w[0] < w[1] { [w[0], w[1]] } else { [w[1], w[0]] });
            }
        }
    }
    segments.sort_unstable();

    // Duplicates cancel in pairs, so an odd count leaves one behind. Three
    // rings meeting along one wall is malformed, but it still has an edge there.
    let mut kept: Vec<[u64; 2]> = Vec::with_capacity(segments.len());
    let mut i = 0;
    while i < segments.len() {
        let mut j = i;
        while j < segments.len() && segments[j] == segments[i] {
            j += 1;
        }
        if (j - i) % 2 == 1 {
            kept.push(segments[i]);
        }
        i = j;
    }

    // Both endpoints of every surviving segment, so following a ring is a
    // lookup and not a scan.
    let mut at: HashMap<u64, Vec<usize>> = HashMap::new();
    for (k, s) in kept.iter().enumerate() {
        at.entry(s[0]).or_default().push(k);
        at.entry(s[1]).or_default().push(k);
    }

    let mut used = vec![false; kept.len()];
    let mut rings: Vec<Vec<u64>> = Vec::new();
    for start in 0..kept.len() {
        if used[start] {
            continue;
        }
        used[start] = true;
        let first = kept[start][0];
        let mut ring = vec![first, kept[start][1]];
        while ring[ring.len() - 1] != first {
            let tail = ring[ring.len() - 1];
            let next = at.get(&tail)?.iter().copied().find(|&k| !used[k])?;
            used[next] = true;
            let s = kept[next];
            ring.push(if s[0] == tail { s[1] } else { s[0] });
        }
        // A ring needs three distinct corners plus the repeated first node.
        if ring.len() >= 4 {
            rings.push(ring);
        }
    }
    Some(rings)
}

// --- containment ----------------------------------------------------------

/// Ray casting, counting crossings of the horizontal line through `p`.
fn point_in_ring(pts: &[Pt], p: Pt) -> bool {
    let mut inside = false;
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let t = (p[1] - a[1]) / (b[1] - a[1]);
            if p[0] < a[0] + t * (b[0] - a[0]) {
                inside = !inside;
            }
        }
    }
    inside
}

/// Is `inner` inside `outer`?
///
/// Testing a single vertex is not enough: OSM rings routinely share vertices
/// with the ring enclosing them (a lake touching the edge of the wood around
/// it), and a shared vertex lands exactly on the boundary, where ray casting is
/// a coin flip. Sampling several vertices and taking the majority costs a
/// handful of extra crossings tests and makes the answer stable.
fn ring_contains(outer: &Ring, inner: &Ring) -> bool {
    let (o, i) = (outer.bbox, inner.bbox);
    if i[0] < o[0] || i[1] < o[1] || i[2] > o[2] || i[3] > o[3] {
        return false;
    }
    let n = inner.pts.len() - 1; // last point repeats the first
    let samples = n.min(9);
    let step = (n / samples).max(1);
    let (mut hits, mut tried) = (0, 0);
    for k in 0..samples {
        if point_in_ring(&outer.pts, inner.pts[k * step]) {
            hits += 1;
        }
        tried += 1;
    }
    hits * 2 > tried
}

/// Nest rings into polygons by containment depth.
///
/// Sorting by descending area means a ring's containers are always already
/// placed when we reach it, so the *nearest* container is simply the last one
/// found scanning backwards. Even depth is an outer ring, odd depth is a hole
/// in its parent — which is what makes an island inside a lake inside an island
/// come out as two polygons rather than one with a bogus hole.
pub fn classify(rings: Vec<Ring>) -> Vec<Polygon> {
    let mut order: Vec<usize> = (0..rings.len()).collect();
    order.sort_by(|&a, &b| {
        rings[b].area.abs().partial_cmp(&rings[a].area.abs()).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut parent = vec![usize::MAX; rings.len()];
    let mut depth = vec![0usize; rings.len()];
    for (pos, &i) in order.iter().enumerate() {
        for &j in order[..pos].iter().rev() {
            if ring_contains(&rings[j], &rings[i]) {
                parent[i] = j;
                depth[i] = depth[j] + 1;
                break;
            }
        }
    }

    // Outer rings keep their position in the polygon list so holes can be
    // appended to the right one in a single pass.
    let mut slot = vec![usize::MAX; rings.len()];
    let mut polys: Vec<Polygon> = Vec::new();
    for &i in &order {
        if depth[i].is_multiple_of(2) {
            slot[i] = polys.len();
            polys.push(Vec::new());
        }
    }
    for (i, mut ring) in rings.into_iter().enumerate() {
        if depth[i].is_multiple_of(2) {
            ring.orient(true);
            let s = slot[i];
            polys[s].insert(0, ring);
        } else if let Some(&s) = slot.get(parent[i]).filter(|&&s| s != usize::MAX) {
            ring.orient(false);
            polys[s].push(ring);
        }
    }
    polys.retain(|p| !p.is_empty());
    polys
}

/// Largest projected span of the outer rings, in metres.
///
/// A bbox span is always >= sqrt(area), so filtering on it only ever discards
/// features the exact area test would discard too — it just costs nothing.
pub fn outer_span(polys: &[Polygon]) -> f64 {
    let (mut x_lo, mut y_lo) = (f64::INFINITY, f64::INFINITY);
    let (mut x_hi, mut y_hi) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for poly in polys {
        for p in &poly[0].pts {
            let x = p[0] * WORLD / 180.0;
            let y = mercator_y(p[1]);
            x_lo = x_lo.min(x);
            x_hi = x_hi.max(x);
            y_lo = y_lo.min(y);
            y_hi = y_hi.max(y);
        }
    }
    if x_lo.is_infinite() {
        return 0.0;
    }
    (x_hi - x_lo).max(y_hi - y_lo)
}

// --- WKB ------------------------------------------------------------------
// Little-endian, no SRID. DuckDB reads this with ST_GeomFromWKB; the previous
// pipeline shipped the same bytes hex-encoded, which doubled them for nothing.

fn header(out: &mut Vec<u8>, kind: u32) {
    out.push(1); // little-endian
    out.extend_from_slice(&kind.to_le_bytes());
}

fn ring_bytes(out: &mut Vec<u8>, pts: &[Pt]) {
    out.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for p in pts {
        out.extend_from_slice(&p[0].to_le_bytes());
        out.extend_from_slice(&p[1].to_le_bytes());
    }
}

pub fn wkb_linestring(pts: &[Pt]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + 16 * pts.len());
    header(&mut out, 2);
    ring_bytes(&mut out, pts);
    out
}

pub fn wkb_multipolygon(polys: &[Polygon]) -> Vec<u8> {
    let points: usize = polys.iter().flatten().map(|r| r.pts.len()).sum();
    let mut out = Vec::with_capacity(9 + polys.len() * 13 + 16 * points);
    header(&mut out, 6);
    out.extend_from_slice(&(polys.len() as u32).to_le_bytes());
    for poly in polys {
        header(&mut out, 3); // each member polygon carries its own header
        out.extend_from_slice(&(poly.len() as u32).to_le_bytes());
        for ring in poly {
            ring_bytes(&mut out, &ring.pts);
        }
    }
    out
}
