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

use crate::tuning::WORLD;

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
        let mut bbox = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        let mut acc = 0.0;
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            bbox[0] = bbox[0].min(a[0]);
            bbox[1] = bbox[1].min(a[1]);
            bbox[2] = bbox[2].max(a[0]);
            bbox[3] = bbox[3].max(a[1]);
            acc += a[0] * b[1] - b[0] * a[1];
        }
        Ring {
            pts,
            bbox,
            area: acc / 2.0,
        }
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
                segments.push(if w[0] < w[1] {
                    [w[0], w[1]]
                } else {
                    [w[1], w[0]]
                });
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
        rings[b]
            .area
            .abs()
            .partial_cmp(&rings[a].area.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Node "positions" for the assembly tests. Real ones are packed lat/lon
    /// pairs; the assembler only compares them, so any distinct u64s will do.
    fn way(ids: &[u64]) -> Vec<u64> {
        ids.to_vec()
    }

    fn square(x0: f64, y0: f64, x1: f64, y1: f64) -> Ring {
        Ring::new(vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]])
    }

    #[test]
    fn a_closed_way_is_one_ring() {
        let rings = assemble_rings(vec![way(&[1, 2, 3, 4, 1])]).unwrap();
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 5, "closed: first node repeated");
        assert_eq!(rings[0][0], rings[0][4]);
    }

    /// Two members meeting end to end, one of them walked the other way round
    /// -- the ordinary multipolygon case, and the direction must not matter.
    #[test]
    fn open_ways_chain_into_a_ring_whatever_their_direction() {
        let rings = assemble_rings(vec![way(&[1, 2, 3]), way(&[1, 4, 3])]).unwrap();
        assert_eq!(rings.len(), 1);
        let r = &rings[0];
        assert_eq!(r.len(), 5);
        assert_eq!(r[0], r[4]);
        let mut corners = r[..4].to_vec();
        corners.sort();
        assert_eq!(corners, vec![1, 2, 3, 4]);
    }

    /// The bug this module exists to not have: two members that share a wall
    /// describe one shape, not two. The shared segment 2-3 appears in both
    /// squares and must cancel, leaving a single six-cornered outline.
    #[test]
    fn a_shared_wall_cancels_and_the_members_merge() {
        let left = way(&[1, 2, 3, 4, 1]);
        let right = way(&[2, 5, 6, 3, 2]);
        let rings = assemble_rings(vec![left, right]).unwrap();
        assert_eq!(
            rings.len(),
            1,
            "nine adjacent outlines are one ring, not nine"
        );
        let r = &rings[0];
        assert_eq!(r.len(), 7, "six corners plus the repeated first");
        let mut corners = r[..6].to_vec();
        corners.sort();
        assert_eq!(corners, vec![1, 2, 3, 4, 5, 6]);
        // The wall itself is gone: 2 and 3 are never consecutive.
        let wall = r
            .windows(2)
            .any(|w| (w[0] == 2 && w[1] == 3) || (w[0] == 3 && w[1] == 2));
        assert!(!wall, "the shared wall survived: {r:?}");
    }

    /// A member missing from the extract leaves an open chain. libosmium
    /// reports that as a broken area and emits nothing; so do we.
    #[test]
    fn an_unclosable_relation_is_refused() {
        assert!(assemble_rings(vec![way(&[1, 2, 3]), way(&[3, 4, 5])]).is_none());
    }

    /// Consecutive duplicate positions (two OSM nodes at one coordinate) are
    /// not segments, and a "ring" of two points is not a ring.
    #[test]
    fn degenerate_input_produces_no_ring() {
        assert_eq!(assemble_rings(vec![way(&[1, 1, 1])]).unwrap().len(), 0);
        assert_eq!(assemble_rings(vec![way(&[1, 2, 1])]).unwrap().len(), 0);
        assert_eq!(assemble_rings(vec![]).unwrap().len(), 0);
    }

    #[test]
    fn ring_area_is_signed_and_bbox_is_tight() {
        let ccw = square(0.0, 0.0, 2.0, 1.0);
        assert!((ccw.area - 2.0).abs() < 1e-12);
        assert_eq!(ccw.bbox, [0.0, 0.0, 2.0, 1.0]);
        let mut pts = ccw.pts.clone();
        pts.reverse();
        assert!(
            (Ring::new(pts).area + 2.0).abs() < 1e-12,
            "clockwise is negative"
        );
    }

    /// Roles are never consulted: nesting is decided by containment, and a
    /// ring inside a ring is a hole whichever order they arrive in.
    #[test]
    fn a_ring_inside_another_is_its_hole() {
        for order in [[0, 1], [1, 0]] {
            let rings: Vec<Ring> = order
                .iter()
                .map(|&i| {
                    if i == 0 {
                        square(0.0, 0.0, 10.0, 10.0)
                    } else {
                        square(2.0, 2.0, 4.0, 4.0)
                    }
                })
                .collect();
            let polys = classify(rings);
            assert_eq!(polys.len(), 1);
            assert_eq!(polys[0].len(), 2, "one outer, one hole");
            assert!(polys[0][0].area > 0.0, "outer is counter-clockwise");
            assert!(polys[0][1].area < 0.0, "hole is clockwise");
            assert_eq!(polys[0][0].bbox, [0.0, 0.0, 10.0, 10.0]);
        }
    }

    /// An island in a lake in an island: depth 2 is an outer ring again, so the
    /// result is two polygons and not one with a bogus hole.
    #[test]
    fn nesting_alternates_outer_and_hole_by_depth() {
        let polys = classify(vec![
            square(0.0, 0.0, 10.0, 10.0),
            square(1.0, 1.0, 9.0, 9.0),
            square(4.0, 4.0, 6.0, 6.0),
        ]);
        assert_eq!(polys.len(), 2);
        let big = polys
            .iter()
            .find(|p| p[0].bbox == [0.0, 0.0, 10.0, 10.0])
            .unwrap();
        let small = polys
            .iter()
            .find(|p| p[0].bbox == [4.0, 4.0, 6.0, 6.0])
            .unwrap();
        assert_eq!(big.len(), 2, "the lake is the island's hole");
        assert_eq!(small.len(), 1, "the inner island is its own polygon");
    }

    /// Two disjoint rings are two polygons, and neither is a hole.
    #[test]
    fn disjoint_rings_are_separate_polygons() {
        let polys = classify(vec![square(0.0, 0.0, 1.0, 1.0), square(5.0, 5.0, 6.0, 6.0)]);
        assert_eq!(polys.len(), 2);
        assert!(polys.iter().all(|p| p.len() == 1));
    }

    /// A hole that touches its outer ring at a vertex -- a lake reaching the
    /// edge of its wood -- still counts as inside, because containment samples
    /// several vertices rather than trusting one that sits on the boundary.
    #[test]
    fn a_hole_sharing_a_vertex_with_its_outer_is_still_inside() {
        let outer = square(0.0, 0.0, 10.0, 10.0);
        let inner = Ring::new(vec![
            [0.0, 0.0],
            [3.0, 1.0],
            [3.0, 3.0],
            [1.0, 3.0],
            [0.0, 0.0],
        ]);
        let polys = classify(vec![outer, inner]);
        assert_eq!(polys.len(), 1);
        assert_eq!(polys[0].len(), 2);
    }

    #[test]
    fn outer_span_is_the_projected_extent_of_the_outer_rings() {
        // One degree of longitude at the equator is WORLD/180 metres.
        let polys = classify(vec![square(0.0, 0.0, 1.0, 0.5)]);
        let span = outer_span(&polys);
        assert!((span - WORLD / 180.0).abs() < 1.0, "{span}");
        assert_eq!(outer_span(&[]), 0.0);
        // Holes do not count: the span is the outer ring's alone.
        let with_hole = classify(vec![square(0.0, 0.0, 1.0, 0.5), square(0.2, 0.1, 0.3, 0.2)]);
        assert!((outer_span(&with_hole) - span).abs() < 1e-6);
    }

    #[test]
    fn mercator_y_is_odd_monotonic_and_clamped() {
        assert!(mercator_y(0.0).abs() < 1e-6);
        assert!((mercator_y(45.0) + mercator_y(-45.0)).abs() < 1e-6);
        assert!(mercator_y(50.0) > mercator_y(49.0));
        // The pole is clamped to the square plane rather than diverging.
        assert!((mercator_y(90.0) - mercator_y(85.05)).abs() < 1e-6);
        assert!((mercator_y(85.05) - WORLD).abs() < WORLD * 0.001);
    }

    fn u32_at(b: &[u8], p: usize) -> u32 {
        u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
    }

    fn f64_at(b: &[u8], p: usize) -> f64 {
        f64::from_le_bytes(b[p..p + 8].try_into().unwrap())
    }

    /// The WKB layout DuckDB's ST_GeomFromWKB reads: little-endian marker, type,
    /// counts, then coordinate pairs.
    #[test]
    fn wkb_layouts_match_the_spec() {
        let line = wkb_linestring(&[[1.0, 2.0], [3.0, 4.0]]);
        assert_eq!(line.len(), 1 + 4 + 4 + 2 * 16);
        assert_eq!(line[0], 1, "little-endian");
        assert_eq!(u32_at(&line, 1), 2, "LineString");
        assert_eq!(u32_at(&line, 5), 2, "two points");
        assert_eq!(f64_at(&line, 9), 1.0);
        assert_eq!(f64_at(&line, 9 + 24), 4.0);

        let polys = classify(vec![
            square(0.0, 0.0, 10.0, 10.0),
            square(2.0, 2.0, 4.0, 4.0),
        ]);
        let mp = wkb_multipolygon(&polys);
        assert_eq!(u32_at(&mp, 1), 6, "MultiPolygon");
        assert_eq!(u32_at(&mp, 5), 1, "one polygon");
        assert_eq!(mp[9], 1);
        assert_eq!(u32_at(&mp, 10), 3, "Polygon");
        assert_eq!(u32_at(&mp, 14), 2, "two rings");
        assert_eq!(u32_at(&mp, 18), 5, "five points in the outer ring");
        assert_eq!(mp.len(), 9 + 9 + 2 * 4 + 10 * 16);
    }
}
