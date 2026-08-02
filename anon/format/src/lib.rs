//! The zone index: a position in, a zone out, and one rule its callers must not
//! break.
//!
//! # What the thing guarantees
//!
//! A *zone* is a set of at least `k` buildings, and the map from position to
//! zone is a **total, deterministic partition of the plane**. Those three words
//! are the whole design:
//!
//!   * **partition** — every position belongs to exactly one zone, and every
//!     position in a zone yields the same zone. So the set of positions that
//!     could have produced a given answer *is* the zone, which contains `k`
//!     buildings. Anything weaker than a partition silently breaks this: see the
//!     note on quadtrees below, which is the trap this format exists to avoid.
//!   * **deterministic** — the same building always yields the same zone. This
//!     is what makes the index safe to query repeatedly. Additive noise (a
//!     random offset, a planar-Laplace draw) is *not* safe to query repeatedly:
//!     n independent draws around one true position converge on it as 1/sqrt(n),
//!     so a service that jitters per request leaks the building to anyone
//!     patient enough to ask 500 times.
//!   * **at least k buildings** — not "at least so many metres". A 300 m circle
//!     is a neighbourhood in Amiens and one farm on the Causse Méjean, and on
//!     the Causse it names the farm. Density is the only thing that can set the
//!     size, which is why this needs baked data at all and cannot be arithmetic
//!     on the coordinates.
//!
//! # The one rule
//!
//! **Every field of the response must be a function of the zone alone.** The
//! zone id is the only thing the caller learns about the position; anything
//! computed from the position itself is a second channel. Some leaks are
//! obvious (echoing the input, a distance-to-centre); the tempting one is not:
//! a `contains_query: bool`, or a bbox clipped to the query's side of the zone,
//! hands back a bit that the zone id does not contain. [`Zone`] is therefore
//! built from a zone record and never sees a latitude.
//!
//! # How the partition is built
//!
//! Buildings are binned into a fixed Web Mercator grid at [`Index::level`], the
//! occupied cells are put in Hilbert order, and the curve is cut wherever the
//! running building count reaches `k`. **A zone is one interval of that curve**,
//! so a lookup is: project, bin, Hilbert, find the interval.
//!
//! Hilbert order is what makes the intervals compact — consecutive keys are
//! adjacent cells, with none of Morton's jumps across the world at every
//! power-of-two boundary — and cutting a curve is what makes the result a
//! partition without any special case at the city edge.
//!
//! The obvious alternative, an adaptive quadtree ("descend while the cell holds
//! k buildings"), is **wrong**, and subtly: if the answer is a z12 cell because
//! the z13 child holding the position was too sparse, then the answer discloses
//! *which* child, namely the sparse one — and that child has fewer than k
//! buildings by construction. Restricting the split to quads whose four
//! children all clear k fixes the leak and destroys the resolution instead: one
//! empty quadrant of farmland blocks every refinement above it, so the whole
//! city next door inherits a 40 km cell. A cut curve has neither problem.
//!
//! # Why the database is small
//!
//! Because a zone is an interval, the index does not have to *store* a zone —
//! only where each one starts. Everything geometric follows from the pair of
//! breakpoints around it: the shape is the set of grid cells the interval
//! covers, which [`extent_of`] recovers exactly by decomposing the interval into
//! aligned quadtree squares. That is 16 bytes a zone not written down, and it
//! removes the one dishonest thing about storing a box instead: a box drawn
//! around the zone's *buildings* does not contain a position standing in a field
//! between them, and the interval always does.
//!
//! What is left per zone — a breakpoint and three small numbers — is then delta
//! coded in blocks of [`Index::block`] against a sampled skip table, so a lookup
//! binary-searches the skip entries and scans one block: **~6 bytes a zone**, and
//! a few hundred contiguous bytes touched however much the index weighs.
//!
//! Nothing is inflated to answer a query -- the compressed form *is* the queried
//! form, the same way the tile archive's gzip is what goes to the client. What
//! that buys is not disk, which is cheap, but resident memory: a continent's
//! worth of zones small enough to ship inside an application rather than beside
//! one. `cargo run --release -p anon-format --example cost` prints both numbers.

// --- geometry -------------------------------------------------------------

/// Half the width of the Web Mercator plane, in projected metres. Same constant
/// as the tile pipeline's, and it has to be: the bake bins building centroids
/// from projected metres, this bins a request from degrees, and the two have to
/// land in the same cell.
pub const WORLD: f64 = 20037508.342789244;

/// Web Mercator's usable latitude range. Past this the projection runs off to
/// infinity and the grid has no cell to offer.
pub const LAT_LIMIT: f64 = 85.0511287798066;

/// Grid cell holding `lat`/`lon` at `level`, i.e. the XYZ tile coordinates at
/// `z = level`.
pub fn cell_of(level: u32, lat: f64, lon: f64) -> (u32, u32) {
    let n = f64::from(1u32 << level);
    let lat = lat.clamp(-LAT_LIMIT, LAT_LIMIT);
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.to_radians().tan()).asinh() / std::f64::consts::PI) / 2.0 * n;
    let last = (1u32 << level) - 1;
    (
        (x.floor().max(0.0) as u32).min(last),
        (y.floor().max(0.0) as u32).min(last),
    )
}

/// West edge of grid column `x`, in degrees. Pass `x + 1` for the east edge.
pub fn lon_of(level: u32, x: u32) -> f64 {
    f64::from(x) / f64::from(1u32 << level) * 360.0 - 180.0
}

/// North edge of grid row `y`, in degrees. Pass `y + 1` for the south edge.
pub fn lat_of(level: u32, y: u32) -> f64 {
    let t = std::f64::consts::PI * (1.0 - 2.0 * f64::from(y) / f64::from(1u32 << level));
    t.sinh().atan().to_degrees()
}

/// Hilbert index of cell `(x, y)` on a `2^level` square.
///
/// The rotate step subtracts on `u64`, which is what the reference C does on
/// `int`: the value can go negative, only its low bits are ever read again, and
/// two's complement makes that come out right. `wrapping_sub` is that same
/// arithmetic said out loud, and is how the PMTiles reader spells it too.
pub fn hilbert(level: u32, x: u32, y: u32) -> u64 {
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    let mut d = 0u64;
    let mut s = 1u64 << (level - 1);
    while s > 0 {
        let rx = u64::from(x & s != 0);
        let ry = u64::from(y & s != 0);
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

/// The cell at Hilbert index `d`. Inverse of [`hilbert`].
///
/// Here the rotate cannot go negative -- `x` and `y` are always below `s` when
/// it runs -- so this one is plain subtraction.
pub fn cell_at(level: u32, d: u64) -> (u32, u32) {
    let (mut x, mut y) = (0u64, 0u64);
    let mut t = d;
    let mut s = 1u64;
    while s < 1u64 << level {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x as u32, y as u32)
}

/// The bounding box of Hilbert interval `[a, b)`, in inclusive cell indices.
///
/// This is what lets a zone be stored as two numbers. The curve's defining
/// property is that any *aligned* run of `4^j` indices is exactly one quadtree
/// square of side `2^j` -- so the interval is chopped into maximal aligned runs
/// (at most two per level, so a few dozen), each run is turned into its square,
/// and the squares are unioned.
///
/// Finding a run's square needs no bookkeeping about which way the curve was
/// facing: any cell inside an aligned square, masked down to a multiple of the
/// side, is the square's corner.
fn quads_of(level: u32, a: u64, b: u64, mut each: impl FnMut(u32, u32, u32)) {
    let mut at = a;
    while at < b {
        // The largest aligned run starting at `at` that still fits in [a, b), as
        // the smaller of what `at`'s alignment allows and what the remaining
        // length affords: 4^j divides `at` for j up to half its trailing zeros,
        // and fits for j up to floor(log4(b - at)).
        let align = if at == 0 {
            level
        } else {
            at.trailing_zeros() / 2
        };
        let fit = (63 - (b - at).leading_zeros()) / 2;
        let j = align.min(fit).min(level);
        let run = 1u64 << (2 * j);
        let side = 1u32 << j;
        let (x, y) = cell_at(level, at);
        each(x & !(side - 1), y & !(side - 1), side);
        at += run;
    }
}

/// Everything geometric about a zone, recovered from its two breakpoints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    /// `[w, s, e, n]` in degrees, and it *contains the position that asked* --
    /// every point in it maps to this zone or, at the edges, to a neighbour.
    pub bbox: [f64; 4],
    /// Centre of the bbox, `[lon, lat]`.
    pub center: [f64; 2],
    /// Ground radius of a circle around the bbox, in metres: how far from
    /// `center` the position can be.
    pub radius_m: f64,
    /// Grid cells in the zone, exactly. Unlike the bbox this is not a bound: it
    /// is the size of the anonymity set in cells.
    pub cells: u64,
    /// Ground area of those cells. Cell area shrinks as cos(lat) and this takes
    /// it at the centre, so a zone spanning several degrees of latitude -- which
    /// only happens in the emptiest places -- is off by a few percent.
    pub area_km2: f64,
}

impl Extent {
    /// Intersect the box with the region the index covers, and recompute what
    /// follows from it.
    ///
    /// This is for the two zones per tier that would otherwise report a box the
    /// size of a hemisphere: the first is stretched back to key 0 and the last
    /// runs to the end of the curve, so both take in every empty cell before or
    /// after the data. Clipping is sound rather than cosmetic -- a position
    /// outside the covered region is refused, so the clipped box still contains
    /// every position this index will *answer* for.
    ///
    /// `cells` and `area_km2` are left describing the whole interval, because
    /// they describe the anonymity set and that is not clipped by anything.
    fn clip(mut self, bounds: [f64; 4]) -> Extent {
        self.bbox = [
            self.bbox[0].max(bounds[0]),
            self.bbox[1].max(bounds[1]),
            self.bbox[2].min(bounds[2]),
            self.bbox[3].min(bounds[3]),
        ];
        (self.center, self.radius_m) = center_and_radius(self.bbox);
        self
    }
}

/// Centre of a box and the ground radius of a circle around it, in metres.
///
/// A degree of latitude is the same distance everywhere; a degree of longitude
/// shrinks as cos(lat), taken at the centre. For a zone whose whole purpose is
/// to be vague, treating the earth as a sphere is precision to spare.
fn center_and_radius(bbox: [f64; 4]) -> ([f64; 2], f64) {
    let center = [(bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0];
    let degree_m = 2.0 * WORLD / 360.0;
    let w_m = (bbox[2] - bbox[0]) * degree_m * center[1].to_radians().cos();
    let h_m = (bbox[3] - bbox[1]) * degree_m;
    (center, (w_m * w_m + h_m * h_m).sqrt() / 2.0)
}

/// The geometry of Hilbert interval `[start, end)` at `level`.
pub fn extent_of(level: u32, start: u64, end: u64) -> Extent {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
    quads_of(level, start, end, |x, y, side| {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + side - 1);
        max_y = max_y.max(y + side - 1);
    });
    let bbox = [
        lon_of(level, min_x),
        lat_of(level, max_y + 1),
        lon_of(level, max_x + 1),
        lat_of(level, min_y),
    ];
    let (center, radius_m) = center_and_radius(bbox);

    // Mercator exaggerates by 1/cos(lat), the same in both axes, so a cell's
    // ground edge is its projected edge times cos(lat).
    let cell_m = 2.0 * WORLD / f64::from(1u32 << level) * center[1].to_radians().cos();
    let cells = end - start;
    Extent {
        bbox,
        center,
        radius_m,
        cells,
        area_km2: cells as f64 * cell_m * cell_m / 1e6,
    }
}

// --- what a place is ------------------------------------------------------

/// Every value [`kind_of`] can return, in increasing density. Exposed so the
/// bake can report by kind without keeping its own copy of the list.
pub const KINDS: [&str; 6] = [
    "wilderness",
    "countryside",
    "village",
    "suburb",
    "city",
    "city-centre",
];

/// How built-up a place is, as a word, from its [`Zone::built_index`].
///
/// The boundaries are calibrated against the baked data, not chosen. Where
/// European landmarks fall is the only test this can be held to, and it is the
/// one that moved these numbers twice:
///
/// ```text
///   Lofoten 0.4   Villers-Bocage 1.2   Chamonix 4.2   Massy 22
///   Amiens 35   Créteil 40   Charing Cross 55   Berlin Mitte 77
///   Amsterdam 85   Châtelet, Grand-Place, Eixample >104
/// ```
///
/// A village comes out low because the window deliberately includes its fields --
/// a village *is* houses with fields around them, and a window tight enough to
/// see only the houses would call it a city.
///
/// They are labels for a number, not a land-use classification: a logistics park
/// reads as "city".
pub fn kind_of(built_index: f64) -> &'static str {
    match built_index {
        p if p < 0.1 => "wilderness",
        p if p < 3.0 => "countryside",
        p if p < 10.0 => "village",
        p if p < 25.0 => "suburb",
        p if p < 50.0 => "city",
        _ => "city-centre",
    }
}

/// The built index to one byte, and back.
///
/// Square-rooted rather than linear because the interesting boundaries are not
/// evenly spaced: farmland and wilderness part company below 0.1, where a whole
/// unit of resolution would see nothing at all, while at the top a city centre
/// does not care about a point either way. This resolves 0.003 at the bottom and
/// 1.1 at the top, and tops out at 200 -- above every landmark measured, which
/// matters because the first ceiling tried was 104 and Paris, Brussels and
/// Barcelona all sat on it, reported as identical.
fn pack_index(index: f64) -> u8 {
    (index.max(0.0).sqrt() * 18.0).round().min(255.0) as u8
}

fn unpack_index(code: u8) -> f64 {
    let root = f64::from(code) / 18.0;
    root * root
}

/// Buildings per km² to one byte, and back. Square-rooted for the same reason,
/// and it tops out at 7 225 -- past any real neighbourhood.
fn pack_density(per_km2: f64) -> u8 {
    (per_km2.max(0.0).sqrt() * 3.0).round().min(255.0) as u8
}

fn unpack_density(code: u8) -> u32 {
    let root = f64::from(code) / 3.0;
    (root * root).round() as u32
}

// --- file layout ----------------------------------------------------------
//
// A header, a tier table, then per tier a skip table and one byte stream. The
// stream holds, per zone, a varint delta from the previous zone's breakpoint and
// three small numbers; the skip table restarts the deltas every `block` zones so
// a lookup never scans more than one block. Multi-byte fields are little-endian
// and read one at a time rather than by casting a slice, so a mapping at any
// address is safe to read and the file is portable.

const MAGIC: &[u8; 8] = b"ANONZONE";
/// Bump on any change to the layout below.
///
/// The length checks in `parse` will not catch a layout change on their own: a
/// stream read under the wrong rules still decodes to *something*, and it comes
/// out as plausible nonsense rather than an error. Which is the hazard the
/// PMTiles reader's `tile_id` comment describes, and has the same answer -- the
/// two ends agree exactly, or they refuse to talk.
const VERSION: u32 = 3;
/// magic, version, level, tier count, block, reserved, then the data bounds.
const HEADER_LEN: usize = 56;
/// k, zone count, offset of the skip table, offset of the stream.
const TIER_LEN: usize = 24;
/// First breakpoint of a block, and where the block starts in the stream.
const SKIP_LEN: usize = 12;

/// One privacy level: a complete partition of the plane into zones of >= `k`
/// buildings. Several live in one file so that changing k is a config change
/// rather than a re-bake, which matters because the choice belongs to the
/// operator -- see [`Index::tier`].
#[derive(Clone, Copy, Debug)]
pub struct Tier {
    pub k: u32,
    pub zones: u32,
    skips: u64,
    stream: u64,
}

impl Tier {
    fn blocks(&self, block: u32) -> u32 {
        self.zones.div_ceil(block)
    }
}

/// A parsed index. Holds no borrow of the mapping: the caller owns the bytes and
/// passes them back in, which is what lets a server keep an `Mmap` and an `Index`
/// in the same struct without a self-reference.
#[derive(Clone, Debug)]
pub struct Index {
    /// Grid level the zones are cut on: cells are `2^level` to a world side.
    pub level: u32,
    /// Zones per delta-coded block. A lookup scans at most one block.
    pub block: u32,
    pub tiers: Vec<Tier>,
    /// Bounding box of the baked data, in degrees, `[w, s, e, n]`.
    ///
    /// Used for one thing only: refusing positions the bake has no buildings
    /// for, rather than answering with the nearest zone on the wrong continent.
    /// That refusal does disclose "outside the covered region" about a position,
    /// which is a deliberate, coarse, documented exception to the rule at the
    /// top of this file -- the caller already knows which region it deployed.
    pub bounds: [f64; 4],
}

/// A zone. Every field is a function of the index and the zone's own two
/// breakpoints; none is a function of the position that looked it up.
#[derive(Clone, Copy, Debug)]
pub struct Zone {
    /// The zone's first Hilbert key. Stable across queries and across processes,
    /// unique within its tier, and usable as a grouping key by a caller that
    /// should not be handling coordinates at all.
    pub id: u64,
    /// How many buildings a position in this zone could be at. `>= k`.
    pub buildings: u32,
    pub k: u32,
    /// Where the zone is and how big, all of it recovered from the breakpoints.
    pub extent: Extent,
    /// Buildings per square kilometre where this zone's buildings are, measured
    /// on a fixed ~2.7 km² window at bake time and weighted by building, then
    /// quantised to a byte (~3% steps).
    ///
    /// Measured, not inferred from `radius_m`: radius answers "how vague is this"
    /// and scales as sqrt(k / density), so it says as much about the operator's
    /// choice of k as about the place. This says only what the place is.
    pub density_per_km2: u32,
    /// How much building there is per unit of ground, on the same window: the
    /// buildings' total bounding-box area as a percentage of the window's area.
    ///
    /// This is the one that tracks *city*, because a count does not: Charing
    /// Cross has few buildings per km² and they are enormous, so by count it
    /// reads as thinner than a housing estate in Massy. Hence [`kind`] comes from
    /// here and not from [`density_per_km2`], and both are reported because they
    /// disagree in interesting places.
    ///
    /// An index and not a coverage fraction, which is why it is not called one:
    /// it is measured from bounding boxes rather than footprints, and in dense
    /// fabric those overlap, so central Paris comes out above 100. Reading `geom`
    /// for 121M polygons to get a true coverage percentage costs two orders of
    /// magnitude more than the four numbers the bake already reads -- worth it
    /// for a number a caller displays, not for a six-way label. Since the
    /// thresholds are calibrated on this same measure, the bias lands in the
    /// number and not in the label.
    ///
    /// [`kind`]: Zone::kind
    /// [`density_per_km2`]: Zone::density_per_km2
    pub built_index: f64,
    /// [`built_index`] as a word.
    ///
    /// [`built_index`]: Zone::built_index
    pub kind: &'static str,
}

impl Index {
    pub fn parse(bytes: &[u8]) -> Result<Index, String> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("not an anon-zone index".into());
        }
        let version = u32_at(bytes, 8);
        if version != VERSION {
            return Err(format!(
                "index version {version} unsupported, expected {VERSION}"
            ));
        }
        let level = u32_at(bytes, 12);
        // 24 keeps a Hilbert key inside 48 bits; 1 is the smallest grid with a
        // curve at all.
        if !(1..=24).contains(&level) {
            return Err(format!("index level {level} out of range"));
        }
        let block = u32_at(bytes, 16);
        if block == 0 {
            return Err("index block size is zero".into());
        }
        let n = u32_at(bytes, 20) as usize;
        let bounds = [
            f64_at(bytes, 24),
            f64_at(bytes, 32),
            f64_at(bytes, 40),
            f64_at(bytes, 48),
        ];
        if bytes.len() < HEADER_LEN + n * TIER_LEN {
            return Err("index truncated in the tier table".into());
        }
        let mut tiers = Vec::with_capacity(n);
        for i in 0..n {
            let at = HEADER_LEN + i * TIER_LEN;
            let tier = Tier {
                k: u32_at(bytes, at),
                zones: u32_at(bytes, at + 4),
                skips: u64_at(bytes, at + 8),
                stream: u64_at(bytes, at + 16),
            };
            if tier.zones == 0 {
                return Err(format!("tier k={} has no zones", tier.k));
            }
            // The skip table is indexed without further checks, so its extent is
            // verified here. The stream is not: it is variable-length, and every
            // read from it is bounds-checked as it goes.
            let skips_end = tier
                .skips
                .checked_add(u64::from(tier.blocks(block)) * SKIP_LEN as u64);
            if skips_end.is_none_or(|end| end > bytes.len() as u64)
                || tier.stream > bytes.len() as u64
            {
                return Err(format!("index truncated in tier k={}", tier.k));
            }
            tiers.push(tier);
        }
        if tiers.is_empty() {
            return Err("index has no tiers".into());
        }
        Ok(Index {
            level,
            block,
            tiers,
            bounds,
        })
    }

    /// The tier for `k`, or the most private tier baked if `k` is `None`.
    ///
    /// Defaulting upward is on purpose, and so is taking k from the operator
    /// rather than from the request. Tiers nest: a position's k=16 zone lies
    /// inside its k=256 zone, so two callers who compare notes about one
    /// position are left with the *smallest* k either of them was given. A
    /// per-request `?k=` therefore does not offer a choice, it offers whoever
    /// asks the smallest k on the menu.
    pub fn tier(&self, k: Option<u32>) -> Option<usize> {
        match k {
            Some(k) => self.tiers.iter().position(|t| t.k == k),
            None => self
                .tiers
                .iter()
                .enumerate()
                .max_by_key(|(_, t)| t.k)
                .map(|(i, _)| i),
        }
    }

    /// Whether the bake has data for this position. See [`Index::bounds`] for
    /// why this exists and what it costs.
    pub fn covers(&self, lat: f64, lon: f64) -> bool {
        let [w, s, e, n] = self.bounds;
        (w..=e).contains(&lon) && (s..=n).contains(&lat)
    }

    /// **The function.** A position, and the compressed index it was baked into;
    /// out comes the zone that stands in for it.
    ///
    /// `bytes` is the file [`parse`] read and nothing else is consulted: no
    /// database, no geometry library, no decompression pass. The work is a
    /// projection, a Hilbert index, a binary search over the skip table and a
    /// scan of one block -- so a few hundred contiguous bytes, whatever the
    /// index weighs.
    ///
    /// `None` only if `tier` is not one of this index's, or if the stream is
    /// corrupt where it was read.
    ///
    /// [`parse`]: Index::parse
    pub fn zone(&self, bytes: &[u8], tier: usize, lat: f64, lon: f64) -> Option<Zone> {
        let t = *self.tiers.get(tier)?;
        let (x, y) = cell_of(self.level, lat, lon);
        let key = hilbert(self.level, x, y);

        // The block whose first breakpoint is the last one <= key. Block 0
        // starts at 0, so this always lands: the partition is total.
        let blocks = t.blocks(self.block);
        let (mut lo, mut hi) = (0u32, blocks);
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.skip(bytes, &t, mid).0 <= key {
                lo = mid;
            } else {
                hi = mid;
            }
        }

        // Scan the block, keeping the last zone that starts at or before `key`
        // and stopping at the first one that starts after it -- that one's
        // breakpoint is this zone's end.
        let (first_key, offset) = self.skip(bytes, &t, lo);
        let mut at = (t.stream + u64::from(offset)) as usize;
        let mut start = first_key;
        let mut found: Option<(u64, u32, u8, u8)> = None;
        let mut end = if lo + 1 < blocks {
            self.skip(bytes, &t, lo + 1).0
        } else {
            // Past the last zone the curve simply runs out.
            1u64 << (2 * self.level)
        };
        let in_block = (t.zones - lo * self.block).min(self.block);
        for i in 0..in_block {
            if i > 0 {
                start += varint(bytes, &mut at)?;
            }
            let buildings = varint(bytes, &mut at)?;
            let (density, built) = (*bytes.get(at)?, *bytes.get(at + 1)?);
            at += 2;
            if start > key {
                end = start;
                break;
            }
            found = Some((start, buildings as u32, density, built));
        }

        let (id, buildings, density, built) = found?;
        let built_index = unpack_index(built);
        Some(Zone {
            id,
            buildings,
            k: t.k,
            extent: extent_of(self.level, id, end).clip(self.bounds),
            density_per_km2: unpack_density(density),
            built_index,
            kind: kind_of(built_index),
        })
    }

    fn skip(&self, bytes: &[u8], tier: &Tier, block: u32) -> (u64, u32) {
        let at = tier.skips as usize + block as usize * SKIP_LEN;
        (u64_at(bytes, at), u32_at(bytes, at + 8))
    }
}

impl Zone {
    /// JSON, by hand, because one object of ten fields is not worth a serialiser
    /// in a process that is otherwise a mmap and a binary search.
    pub fn to_json(&self) -> String {
        let e = &self.extent;
        format!(
            concat!(
                r#"{{"zone":"{:x}","k":{},"buildings":{},"#,
                r#""bbox":[{:.6},{:.6},{:.6},{:.6}],"center":[{:.6},{:.6}],"#,
                r#""radius_m":{:.0},"area_km2":{:.3},"cells":{},"#,
                r#""density_per_km2":{},"built_index":{:.2},"kind":"{}"}}"#
            ),
            self.id,
            self.k,
            self.buildings,
            e.bbox[0],
            e.bbox[1],
            e.bbox[2],
            e.bbox[3],
            e.center[0],
            e.center[1],
            e.radius_m,
            e.area_km2,
            e.cells,
            self.density_per_km2,
            self.built_index,
            self.kind,
        )
    }
}

// --- cutting --------------------------------------------------------------

/// One occupied grid cell, as the bake hands it over: how many buildings are in
/// it and what the neighbourhood around it looks like.
#[derive(Clone, Copy, Debug)]
pub struct Bin {
    /// Hilbert index of the cell at the index's level.
    pub key: u64,
    pub buildings: u32,
    /// Buildings per km² over the surrounding window.
    pub density: f32,
    /// Percentage of ground covered by buildings over the surrounding window.
    pub built: f32,
}

/// One zone as the bake accumulates it, and as [`encode`] writes it.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    /// First Hilbert key of the zone. The zone runs to the next record's start.
    pub start: u64,
    pub buildings: u32,
    /// Building-weighted sums, divided by `buildings` on the way out -- which is
    /// why they are wider than the bytes they end up in.
    pub density_sum: f64,
    pub built_sum: f64,
}

impl Record {
    fn new(start: u64) -> Record {
        Record {
            start,
            buildings: 0,
            density_sum: 0.0,
            built_sum: 0.0,
        }
    }

    fn absorb(&mut self, bin: &Bin) {
        self.buildings += bin.buildings;
        self.density_sum += f64::from(bin.density) * f64::from(bin.buildings);
        self.built_sum += f64::from(bin.built) * f64::from(bin.buildings);
    }

    /// Absorb a zone that follows this one on the curve. Used once per tier, on
    /// the tail end that did not reach k.
    fn absorb_zone(&mut self, other: &Record) {
        self.buildings += other.buildings;
        self.density_sum += other.density_sum;
        self.built_sum += other.built_sum;
    }

    /// Weighted mean density, i.e. the density where this zone's candidates
    /// actually are -- a zone straddling a village and its fields reports the
    /// village, because that is where its buildings are.
    pub fn density(&self) -> f64 {
        self.mean(self.density_sum)
    }

    /// Weighted mean built-up percentage.
    pub fn built(&self) -> f64 {
        self.mean(self.built_sum)
    }

    fn mean(&self, sum: f64) -> f64 {
        if self.buildings == 0 {
            return 0.0;
        }
        sum / f64::from(self.buildings)
    }
}

/// Cut the curve into zones of at least `k` buildings. `bins` must be sorted by
/// `key`, which is what makes a zone a contiguous piece of the plane.
///
/// Three details carry the guarantee, and all three are easy to leave out:
///
///   * The first zone is stretched back to key 0, so the stretch of curve before
///     any building belongs to somebody. The map has to be total, including over
///     the sea -- a position with no zone is a position the service would have to
///     say something else about.
///   * The tail, which by definition did not reach k, is folded into its
///     predecessor rather than shipped as a zone naming too few buildings.
///   * Cells are atomic: a cut never falls inside one. That is what makes "the
///     same cell always answers the same zone" true, and it is also why a zone
///     can overshoot k, since one dense cell can carry hundreds of buildings.
pub fn cut(bins: &[Bin], k: u32) -> Vec<Record> {
    let mut zones: Vec<Record> = Vec::new();
    let mut open: Option<Record> = None;
    for b in bins {
        let z = open.get_or_insert_with(|| Record::new(b.key));
        z.absorb(b);
        if z.buildings >= k {
            zones.push(open.take().expect("just filled"));
        }
    }
    if let Some(tail) = open.take() {
        match zones.last_mut() {
            Some(prev) => prev.absorb_zone(&tail),
            // Fewer than k buildings in the whole bake: one zone, and `encode`
            // lets it through short because there is nothing to merge it into.
            None => zones.push(tail),
        }
    }
    if let Some(first) = zones.first_mut() {
        first.start = 0;
    }
    zones
}

// --- writing --------------------------------------------------------------

/// Serialise a whole index. `tiers` is `(k, zones)`, each zone list sorted by
/// `start` and each a complete partition -- which is checked here, because a
/// partition with a hole in it is a privacy bug and this is the last place it can
/// be caught cheaply.
pub fn encode(
    level: u32,
    block: u32,
    bounds: [f64; 4],
    tiers: &[(u32, Vec<Record>)],
) -> Result<Vec<u8>, String> {
    if !(1..=24).contains(&level) {
        return Err(format!("level {level} out of range"));
    }
    if block == 0 {
        return Err("block size must be positive".into());
    }
    for (k, zones) in tiers {
        if zones.is_empty() {
            return Err(format!("tier k={k} is empty"));
        }
        if zones[0].start != 0 {
            return Err(format!("tier k={k} does not start at 0, leaving a gap"));
        }
        if zones.last().expect("non-empty").start >= 1u64 << (2 * level) {
            return Err(format!("tier k={k} has a zone past the end of the curve"));
        }
        for pair in zones.windows(2) {
            if pair[1].start <= pair[0].start {
                return Err(format!("tier k={k} is not sorted by start"));
            }
        }
        // The last zone may fall short only if the whole tier is one zone: with
        // fewer than k buildings baked there is nothing else to merge it into.
        if let Some(bad) = zones.iter().find(|z| z.buildings < *k && zones.len() > 1) {
            return Err(format!(
                "tier k={k} has a zone of {} buildings at {:x}",
                bad.buildings, bad.start
            ));
        }
    }

    // The payload streams first, so the tier table can point at them.
    let mut skips: Vec<Vec<u8>> = Vec::new();
    let mut streams: Vec<Vec<u8>> = Vec::new();
    for (k, zones) in tiers {
        let (mut skip, mut stream) = (Vec::new(), Vec::new());
        for (i, z) in zones.iter().enumerate() {
            if i % block as usize == 0 {
                let offset = u32::try_from(stream.len())
                    .map_err(|_| format!("tier k={k} stream exceeds 4 GB"))?;
                skip.extend_from_slice(&z.start.to_le_bytes());
                skip.extend_from_slice(&offset.to_le_bytes());
            } else {
                put_varint(&mut stream, z.start - zones[i - 1].start);
            }
            // The count itself, not the overshoot above k. The overshoot would be
            // a byte smaller for k >= 128 and cannot express the one zone that is
            // allowed to fall short of k -- which would then read back as exactly
            // k, overstating the anonymity of the only case where it is thin.
            put_varint(&mut stream, u64::from(z.buildings));
            stream.push(pack_density(z.density()));
            stream.push(pack_index(z.built()));
        }
        skips.push(skip);
        streams.push(stream);
    }

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&level.to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&(tiers.len() as u32).to_le_bytes());
    for v in bounds {
        out.extend_from_slice(&v.to_le_bytes());
    }
    debug_assert_eq!(out.len(), HEADER_LEN);

    let mut at = (HEADER_LEN + tiers.len() * TIER_LEN) as u64;
    for (i, (k, zones)) in tiers.iter().enumerate() {
        out.extend_from_slice(&k.to_le_bytes());
        out.extend_from_slice(&(zones.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&(at + skips[i].len() as u64).to_le_bytes());
        at += (skips[i].len() + streams[i].len()) as u64;
    }
    for (skip, stream) in skips.iter().zip(&streams) {
        out.extend_from_slice(skip);
        out.extend_from_slice(stream);
    }
    Ok(out)
}

// --- bytes ----------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// LEB128, bounds-checked: this reads a file that a request steered us into, so
/// a truncated stream has to come back as `None` and not as a panic.
fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let (mut v, mut shift) = (0u64, 0u32);
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn f64_at(b: &[u8], at: usize) -> f64 {
    f64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORLD_BOUNDS: [f64; 4] = [-180.0, -85.0, 180.0, 85.0];

    #[test]
    fn hilbert_is_a_permutation_and_a_walk() {
        // Every cell exactly once, consecutive keys on adjacent cells, and the
        // inverse agreeing throughout -- the properties the whole partition
        // leans on, since a zone is an interval of this curve and is only
        // compact if the curve is local.
        for level in 1..=5 {
            let n = 1u32 << level;
            let mut seen = vec![None; (n as usize) * (n as usize)];
            for y in 0..n {
                for x in 0..n {
                    let d = hilbert(level, x, y);
                    assert_eq!(cell_at(level, d), (x, y), "level {level}: inverse");
                    let slot = &mut seen[d as usize];
                    assert!(slot.is_none(), "level {level}: key {d} twice");
                    *slot = Some((x, y));
                }
            }
            for pair in seen.windows(2) {
                let (a, b) = (pair[0].unwrap(), pair[1].unwrap());
                let step = a.0.abs_diff(b.0) + a.1.abs_diff(b.1);
                assert_eq!(step, 1, "level {level}: jump from {a:?} to {b:?}");
            }
        }
    }

    /// The claim that lets a zone be stored as two numbers: the interval's box,
    /// derived by decomposing it into aligned squares, is the same box you get by
    /// walking every cell in it.
    #[test]
    fn interval_boxes_match_brute_force() {
        // Every interval of every grid up to 16x16: 32 896 of them at level 4,
        // which is exhaustive and still quick. Level 5 is 16x the intervals and
        // 4x the cells in each, and proves nothing more.
        for level in 1..=4 {
            let n = 1u64 << (2 * level);
            for a in 0..n {
                for b in a + 1..=n {
                    let (mut bx, mut by, mut mx, mut my) = (u32::MAX, u32::MAX, 0, 0);
                    for d in a..b {
                        let (x, y) = cell_at(level, d);
                        bx = bx.min(x);
                        by = by.min(y);
                        mx = mx.max(x);
                        my = my.max(y);
                    }
                    let e = extent_of(level, a, b);
                    assert_eq!(e.cells, b - a);
                    assert_eq!(
                        [
                            lon_of(level, bx),
                            lat_of(level, my + 1),
                            lon_of(level, mx + 1),
                            lat_of(level, by)
                        ],
                        e.bbox,
                        "level {level}, interval [{a},{b})"
                    );
                }
            }
        }
    }

    #[test]
    fn cells_and_edges_agree() {
        // A cell's own edges have to land back in it, or a zone's bbox would not
        // contain the cells it was cut from.
        let level = 18;
        for (lat, lon) in [(48.85, 2.35), (49.89, 2.30), (-33.87, 151.21), (0.0, 0.0)] {
            let (x, y) = cell_of(level, lat, lon);
            let (w, n) = (lon_of(level, x), lat_of(level, y));
            let (e, s) = (lon_of(level, x + 1), lat_of(level, y + 1));
            assert!(w <= lon && lon < e, "{lon} not in [{w},{e})");
            assert!(s < lat && lat <= n, "{lat} not in ({s},{n}]");
            assert_eq!(cell_of(level, (n + s) / 2.0, (w + e) / 2.0), (x, y));
        }
    }

    /// A deterministic pseudo-random continent: dense in one corner, thinning
    /// out, empty over most of it.
    fn continent(level: u32) -> Vec<Bin> {
        let n = 1u32 << level;
        let mut bins = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let h = u64::from(x)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(u64::from(y).wrapping_mul(1_442_695_040_888_963_407))
                    >> 33;
                let buildings = match (x < n / 5 && y < n / 5, h % 7) {
                    (true, _) => 20 + (h % 90) as u32,
                    (false, 0) => 1 + (h % 3) as u32,
                    _ => 0,
                };
                if buildings > 0 {
                    bins.push(Bin {
                        key: hilbert(level, x, y),
                        buildings,
                        density: (buildings * 40) as f32,
                        built: f32::from(buildings as u16) / 4.0,
                    });
                }
            }
        }
        bins.sort_by_key(|b| b.key);
        bins
    }

    /// The guarantee, over *every* cell of the grid rather than a sample --
    /// because the failure being guarded against is a leak on the sparse fringe,
    /// which is exactly where a sample would not look.
    #[test]
    fn every_position_is_hidden_among_k_buildings() {
        let level = 6;
        let n = 1u32 << level;
        let bins = continent(level);
        let total: u32 = bins.iter().map(|b| b.buildings).sum();

        for k in [2, 16, 64, 512] {
            for block in [1, 7, 64] {
                let zones = cut(&bins, k);
                let bytes = encode(level, block, WORLD_BOUNDS, &[(k, zones.clone())]).unwrap();
                let ix = Index::parse(&bytes).unwrap();

                let mut answers = std::collections::HashMap::new();
                for y in 0..n {
                    for x in 0..n {
                        let lat = (lat_of(level, y) + lat_of(level, y + 1)) / 2.0;
                        let lon = (lon_of(level, x) + lon_of(level, x + 1)) / 2.0;
                        let z = ix.zone(&bytes, 0, lat, lon).expect("total");
                        assert!(
                            z.buildings >= k.min(total),
                            "k={k} block={block}: ({x},{y}) hidden among only {}",
                            z.buildings
                        );
                        // The box has to contain the position that asked, which
                        // is the property a box around the zone's buildings
                        // would not have.
                        let [w, s, e, nn] = z.extent.bbox;
                        assert!(
                            w <= lon && lon <= e && s <= lat && lat <= nn,
                            "k={k}: ({x},{y}) outside its own zone {:?}",
                            z.extent.bbox
                        );
                        answers.insert((x, y), z.id);
                    }
                }

                // Every zone reachable, and no answer that is not a zone: the
                // count of distinct ids has to match the partition exactly.
                let ids: std::collections::HashSet<_> = answers.values().copied().collect();
                assert_eq!(
                    ids.len(),
                    zones.len(),
                    "k={k} block={block}: partition has a hole"
                );
                assert!(zones.iter().all(|z| ids.contains(&z.start)));
            }
        }
    }

    /// The compressed stream has to give back exactly what went in -- the counts
    /// and both measures, within the quantiser's documented resolution.
    /// Clipping the reported box to the covered region must not clip away a
    /// position the index still answers for. It cannot -- a position inside both
    /// the zone and the bounds is inside their intersection -- and this is the
    /// test that says so out loud, over the whole grid.
    #[test]
    fn clipping_keeps_every_answerable_position_inside_its_box() {
        let level = 6;
        let n = 1u32 << level;
        let bins = continent(level);
        // A region covering the dense corner and a little slack, so most of the
        // grid is out of bounds and the clip actually bites.
        let bounds = [
            lon_of(level, 0),
            lat_of(level, n / 3),
            lon_of(level, n / 3),
            lat_of(level, 0),
        ];
        let bytes = encode(level, 64, bounds, &[(32, cut(&bins, 32))]).unwrap();
        let ix = Index::parse(&bytes).unwrap();

        let mut inside = 0;
        for y in 0..n {
            for x in 0..n {
                let lat = (lat_of(level, y) + lat_of(level, y + 1)) / 2.0;
                let lon = (lon_of(level, x) + lon_of(level, x + 1)) / 2.0;
                if !ix.covers(lat, lon) {
                    continue;
                }
                inside += 1;
                let z = ix.zone(&bytes, 0, lat, lon).expect("total");
                let [w, s, e, nn] = z.extent.bbox;
                assert!(
                    w <= lon && lon <= e && s <= lat && lat <= nn,
                    "({x},{y}) clipped out of its own zone {:?}",
                    z.extent.bbox
                );
            }
        }
        assert!(
            inside > 100,
            "the covered region should hold most of a corner"
        );
    }

    #[test]
    fn payloads_survive_the_round_trip() {
        let level = 6;
        let bins = continent(level);
        let zones = cut(&bins, 32);
        let bytes = encode(level, 64, WORLD_BOUNDS, &[(32, zones.clone())]).unwrap();
        let ix = Index::parse(&bytes).unwrap();

        for want in &zones {
            let (x, y) = cell_at(level, want.start);
            let lat = (lat_of(level, y) + lat_of(level, y + 1)) / 2.0;
            let lon = (lon_of(level, x) + lon_of(level, x + 1)) / 2.0;
            let got = ix.zone(&bytes, 0, lat, lon).expect("its own zone");
            assert_eq!(got.id, want.start);
            assert_eq!(got.buildings, want.buildings);
            // Square-root quantised to a byte: 3% on density, 0.4 points at the
            // top of the built-up range and far finer at the bottom.
            assert!(
                (f64::from(got.density_per_km2) - want.density()).abs()
                    <= want.density() * 0.03 + 1.0,
                "density {} vs {}",
                got.density_per_km2,
                want.density()
            );
            assert!(
                (got.built_index - want.built()).abs() <= 1.2,
                "built index {} vs {}",
                got.built_index,
                want.built()
            );
        }
    }

    #[test]
    fn nonsense_files_are_refused_not_misread() {
        let bins = continent(4);
        let zones = cut(&bins, 16);
        let good = encode(4, 8, WORLD_BOUNDS, &[(16, zones)]).unwrap();
        assert!(Index::parse(&good).is_ok());

        assert!(Index::parse(b"").is_err());
        assert!(Index::parse(&good[..40]).is_err());
        for (at, to) in [(8, 9u8), (12, 99), (16, 0)] {
            let mut bad = good.clone();
            bad[at] = to;
            assert!(
                Index::parse(&bad).is_err(),
                "byte {at} -> {to} should be refused"
            );
        }
        // Truncated mid-stream: parse cannot see it, so the lookup has to.
        let ix = Index::parse(&good).unwrap();
        let short = &good[..good.len() - 4];
        let mut refused = 0;
        for x in 0..16u32 {
            if ix.zone(short, 0, 10.0, lon_of(4, x) + 1.0).is_none() {
                refused += 1;
            }
        }
        assert!(refused > 0, "a truncated stream must fail some lookup");
    }

    #[test]
    fn a_gap_in_the_partition_is_refused() {
        let mut zones = cut(&continent(4), 16);
        zones[0].start = 1; // no longer covers the start of the curve
        assert!(encode(4, 8, WORLD_BOUNDS, &[(16, zones)]).is_err());

        let mut zones = cut(&continent(4), 16);
        zones.swap(1, 2); // out of order
        assert!(encode(4, 8, WORLD_BOUNDS, &[(16, zones)]).is_err());

        let mut zones = cut(&continent(4), 16);
        zones[1].buildings = 3; // below k, and not the tail
        assert!(encode(4, 8, WORLD_BOUNDS, &[(16, zones)]).is_err());
    }
}
