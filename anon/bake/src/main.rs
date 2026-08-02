//! Cut the world into zones of k buildings.
//!
//! Reads building footprints out of the tile pipeline's `features` table, bins
//! their centroids on a fixed Web Mercator grid, walks the occupied cells in
//! Hilbert order and starts a new zone every time the running count reaches k.
//! Writes one mmappable index holding every requested k. See `anon-format` for
//! what the result guarantees and why it is a cut curve rather than a quadtree.
//!
//!   anon-bake                                  # defaults, whole database
//!   anon-bake --k 16,64,256 --level 18
//!   anon-bake --bbox 1.7 49.7 2.7 50.1         # one region, for a quick look
//!
//! ## What counts as a building
//!
//! Every OSM building, which is a proxy and worth being explicit about. It errs
//! in one direction that matters: a hamlet of three houses and thirty barns
//! counts as thirty-three, so a k=32 zone there could be one family. If the
//! positions being anonymised are people at home, pass `--min-footprint 25` to
//! drop sheds, or better, bake against addresses or a population grid -- the
//! count per cell is the only thing this program wants from the data, and
//! swapping what fills it does not change anything downstream.
//!
//! It also errs harmlessly in the other direction: a 200-flat tower counts once,
//! so zones in dense cities hold far more people than k suggests.

use std::time::Instant;

use anon_format::{self as fmt, Record};
use rayon::slice::ParallelSliceMut;

/// Grid the zones are cut on. Cells are `2^LEVEL` to a world side: at z18 that
/// is 153 m at the equator and 102 m at Amiens, which sets the finest a zone can
/// possibly be. Going finer costs a bigger sort for resolution no privacy
/// argument wants -- a 25 m zone is a building.
const LEVEL: u32 = 18;

/// The privacy ladder, in buildings per zone. Roughly: a few blocks, a
/// neighbourhood, a district. Baking several is cheap and means the operator can
/// change its mind without re-reading 121M footprints; it is not an invitation
/// to let the caller pick (see `Index::tier`).
const TIERS: &[u32] = &[16, 64, 256];

/// Grid the reported density is measured on, four levels above [`LEVEL`], so
/// 2.7 km² a cell around Amiens.
///
/// This is a separate measurement from the zones and has to be: a zone's own
/// size scales as sqrt(k / density), so reading density back off it would report
/// the operator's k as much as the place. A fixed window says what the place is
/// regardless of k. Wide enough that a village and its fields land in one cell,
/// which is the point -- a village *is* houses with fields around them, and a
/// window tight enough to see only the houses would call it a city.
const DENSITY_LEVEL: u32 = 14;

/// Zones per delta-coded block, which is the compression/latency dial.
///
/// A lookup binary-searches one 12-byte skip entry per block and then scans one
/// block, so bigger blocks mean a smaller skip table and a longer scan. At 64 the
/// scan is ~320 bytes -- five cache lines, less than a single page fault -- and
/// the skip table is a sixth of the index.
const BLOCK: u32 = 64;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args().skip(1))?;
    println!(
        "level z{}, k = {}, from {}",
        args.level,
        args.tiers
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        args.db.display()
    );

    let t0 = Instant::now();
    let (mut cells, sites) = read_cells(&args)?;
    if cells.is_empty() {
        return Err("no buildings matched -- wrong database, or a bbox off the data".into());
    }
    let buildings: u64 = cells.iter().map(|c| u64::from(c.buildings)).sum();
    timed(
        format!(
            "{} buildings in {} occupied cells, {:.1} per cell",
            commas(buildings),
            commas(cells.len() as u64),
            buildings as f64 / cells.len() as f64
        ),
        t0,
    );

    // Both of these run before the sort, while `sites` still lines up with
    // `cells`; after it, nothing needs a cell's position again.
    let t0 = Instant::now();
    let windows = measure_density(&mut cells, &sites, args.level);
    let bounds = bounds_of(&sites, args.level);
    drop(sites);
    timed(
        format!(
            "density measured over {} windows at z{DENSITY_LEVEL}",
            commas(windows)
        ),
        t0,
    );

    // The one CPU-bound step. Hilbert order is the whole construction: after
    // this, a zone is a slice of the vector.
    let t0 = Instant::now();
    cells.par_sort_unstable_by_key(|c| c.key);
    timed("sorted into Hilbert order", t0);

    // Building-weighted, so this is the distribution as seen by a position being
    // anonymised rather than by a square kilometre of Europe -- which is mostly
    // field, and would put every percentile in the countryside. This is the
    // calibration behind `kind_of`, printed so a different region can check it.
    for (what, unit, seen) in [
        (
            "density ",
            "/km²",
            seen_by_building(&cells, |c| f64::from(c.density)),
        ),
        (
            "built index",
            "",
            seen_by_building(&cells, |c| f64::from(c.built)),
        ),
    ] {
        println!(
            "  a building sees {what} p10 {:.1}, p50 {:.1}, p90 {:.1}, p99 {:.1} {unit}",
            pick(&seen, 0.10),
            pick(&seen, 0.50),
            pick(&seen, 0.90),
            pick(&seen, 0.99),
        );
    }

    println!(
        "  data bounds {:.3} {:.3} .. {:.3} {:.3}",
        bounds[0], bounds[1], bounds[2], bounds[3]
    );

    let mut tiers = Vec::new();
    for &k in &args.tiers {
        let t0 = Instant::now();
        let zones = fmt::cut(&cells, k);
        timed(format!("k={k}: {} zones", commas(zones.len() as u64)), t0);
        // The table the operator actually decides on: what k buys, by the kind of
        // place it is bought in. A single median over all zones hides the whole
        // point, since the countryside contributes most of the zones and none of
        // the population.
        for (kind, radii) in radii_by_kind(&zones, args.level) {
            println!(
                "    {kind:<12} {:>9} zones   radius p50 {:>6.0} m   p90 {:>6.0} m",
                commas(radii.len() as u64),
                pick(&radii, 0.50),
                pick(&radii, 0.90),
            );
        }
        tiers.push((k, zones));
    }

    // `encode` re-checks that each tier is a gapless, sorted, k-clearing
    // partition before it writes a byte. Cheap, and the failure it catches is a
    // privacy bug rather than a crash.
    let bytes = fmt::encode(args.level, BLOCK, bounds, &tiers)?;
    std::fs::write(&args.out, &bytes)?;
    let zones: usize = tiers.iter().map(|(_, z)| z.len()).sum();
    println!(
        "  {:.1} MB -> {} ({:.1} bytes a zone)",
        bytes.len() as f64 / 1e6,
        args.out.display(),
        bytes.len() as f64 / zones as f64,
    );

    // Prove the file answers, before anything is deployed against it.
    let ix = fmt::Index::parse(&bytes)?;
    let tier = ix.tier(None).expect("a tier was baked");
    let (lat, lon) = ((bounds[1] + bounds[3]) / 2.0, (bounds[0] + bounds[2]) / 2.0);
    if let Some(z) = ix.zone(&bytes, tier, lat, lon) {
        println!("  centre of the data reads back as {}", z.to_json());
    }
    Ok(())
}

/// What the bake needs per cell before the Hilbert sort and never after: where
/// the cell is on the grid, and how much building it holds.
///
/// Kept beside the `Bin` array rather than inside it. `Bin` is what the cut
/// consumes, and the sort reorders it -- so anything travelling alongside would
/// be silently unpaired. This is used and dropped before the sort happens, and
/// the position is recoverable from the Hilbert key afterwards anyway.
struct Site {
    x: u32,
    y: u32,
    footprint: f32,
}

/// Fill in each cell's two local measures from the window it sits in, and return
/// how many windows were occupied.
///
/// Two passes over the cells and a hash map keyed on the window: at Europe scale
/// that is 1.4M windows against 35M cells, small enough that the map stays in
/// cache-friendly territory.
fn measure_density(cells: &mut [fmt::Bin], sites: &[Site], level: u32) -> u64 {
    let shift = level - DENSITY_LEVEL;
    let window_of = |s: &Site| u64::from(s.x >> shift) << 32 | u64::from(s.y >> shift);
    let mut totals: std::collections::HashMap<u64, (u32, f64)> = std::collections::HashMap::new();
    for (c, s) in cells.iter().zip(sites) {
        let e = totals.entry(window_of(s)).or_insert((0, 0.0));
        e.0 += c.buildings;
        e.1 += f64::from(s.footprint);
    }
    // A window's ground area shrinks as cos(lat), so the same count is a higher
    // density in Tromsø than in Nice. Cached per row of windows, which share a
    // latitude and of which there are only 2^DENSITY_LEVEL.
    let span = 2.0 * fmt::WORLD / f64::from(1u32 << DENSITY_LEVEL);
    let mut area_of_row = std::collections::HashMap::new();
    for (c, s) in cells.iter_mut().zip(sites) {
        let wy = s.y >> shift;
        let area_m2 = *area_of_row.entry(wy).or_insert_with(|| {
            let lat = (fmt::lat_of(DENSITY_LEVEL, wy) + fmt::lat_of(DENSITY_LEVEL, wy + 1)) / 2.0;
            let edge = span * lat.to_radians().cos();
            edge * edge
        });
        let (n, footprint) = totals[&window_of(s)];
        c.density = (f64::from(n) / (area_m2 / 1e6)) as f32;
        c.built = (footprint / area_m2 * 100.0) as f32;
    }
    totals.len() as u64
}

/// Everything the grid needs from the database: a count and a building area per
/// occupied cell.
///
/// The binning happens in SQL because it turns 121M rows into 35M and because the
/// arithmetic has to match the pipeline's own -- `features` holds bounding boxes
/// in projected metres, and these are the same two expressions
/// `config::cell_sql` uses to place a feature on the tile grid.
fn read_cells(args: &Args) -> Result<(Vec<fmt::Bin>, Vec<Site>), Box<dyn std::error::Error>> {
    let con = connect(args)?;
    let span = 2.0 * fmt::WORLD / f64::from(1u32 << args.level);
    let world = fmt::WORLD;
    let sql = format!(
        "SELECT CAST(FLOOR(((min_x + max_x) / 2 + {world}) / {span}) AS UINTEGER) tx,
                CAST(FLOOR(({world} - (min_y + max_y) / 2) / {span}) AS UINTEGER) ty,
                CAST(count(*) AS UINTEGER) n,
                sum({}) footprint
         FROM features
         WHERE layer = 'buildings'{}{}
         GROUP BY tx, ty",
        footprint_m2(),
        args.bbox_sql(),
        args.footprint_sql(),
    );

    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let (mut cells, mut sites) = (Vec::new(), Vec::new());
    while let Some(r) = rows.next()? {
        let (x, y, n): (u32, u32, u32) = (r.get(0)?, r.get(1)?, r.get(2)?);
        cells.push(fmt::Bin {
            key: fmt::hilbert(args.level, x, y),
            buildings: n,
            // Filled in by `measure_density`, which needs every cell read first.
            density: 0.0,
            built: 0.0,
        });
        sites.push(Site {
            x,
            y,
            footprint: r.get::<_, f64>(3)? as f32,
        });
    }
    Ok((cells, sites))
}

/// Ground area of a building, in m², from its bounding box.
///
/// The box, not the footprint: `geom` is by far the widest column in `features`
/// and reading 121M polygons to call `ST_Area` costs two orders of magnitude more
/// than the four numbers already being read, for a number this only feeds a
/// six-way label. It overstates a diagonal or L-shaped building, and since the
/// thresholds are calibrated on the same measure the bias lands in the number
/// rather than in the label.
///
/// Projected metres overstate ground distance by 1/cos(lat) in each axis, hence
/// the cos² -- without it the same house is twice the building in Tromsø that it
/// is in Nice. Latitude comes back out of the Mercator northing by the inverse
/// of the pipeline's own projection.
fn footprint_m2() -> String {
    let world = fmt::WORLD;
    format!(
        "(max_x - min_x) * (max_y - min_y)
             * pow(cos(atan(sinh(pi() * ((min_y + max_y) / 2) / {world}))), 2)"
    )
}

/// Bounding box of the baked cells, in degrees.
fn bounds_of(sites: &[Site], level: u32) -> [f64; 4] {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    for s in sites {
        min_x = min_x.min(s.x);
        min_y = min_y.min(s.y);
        max_x = max_x.max(s.x);
        max_y = max_y.max(s.y);
    }
    [
        fmt::lon_of(level, min_x),
        fmt::lat_of(level, max_y + 1),
        fmt::lon_of(level, max_x + 1),
        fmt::lat_of(level, min_y),
    ]
}

/// Zone radii in metres, sorted, grouped by the kind of place -- what the
/// operator actually wants to see, since "k=64" means nothing until you know it
/// buys 300 m in Lille and 4 km in the Morvan.
///
/// Ordered densest first, which is the order the zones matter in: the top row is
/// where most of the positions being anonymised will land.
fn radii_by_kind(zones: &[Record], level: u32) -> Vec<(&'static str, Vec<f64>)> {
    let mut by_kind: std::collections::HashMap<&str, Vec<f64>> = std::collections::HashMap::new();
    for (i, z) in zones.iter().enumerate() {
        // A zone runs to the next one's breakpoint, and the last runs to the end
        // of the curve -- the same reconstruction the lookup does.
        let end = zones
            .get(i + 1)
            .map_or(1u64 << (2 * level), |next| next.start);
        by_kind
            .entry(fmt::kind_of(z.built()))
            .or_default()
            .push(fmt::extent_of(level, z.start, end).radius_m);
    }
    let mut out: Vec<_> = fmt::KINDS
        .iter()
        .rev()
        .filter_map(|k| by_kind.remove_entry(k))
        .collect();
    for (_, radii) in &mut out {
        radii.sort_by(f64::total_cmp);
    }
    out
}

/// A per-cell measure's distribution one building at a time, sorted: expanding
/// `cells` by building count would be the literal thing, so this weights instead
/// and reads the quantile off the running total.
fn seen_by_building(cells: &[fmt::Bin], of: impl Fn(&fmt::Bin) -> f64) -> Vec<f64> {
    let mut pairs: Vec<(f64, u32)> = cells.iter().map(|c| (of(c), c.buildings)).collect();
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: u64 = pairs.iter().map(|p| u64::from(p.1)).sum();
    // Expand to a fixed 1000-bucket ladder so `pick` can index it directly.
    let mut out = Vec::with_capacity(1000);
    let (mut seen, mut at) = (0u64, 0usize);
    for (d, n) in pairs {
        seen += u64::from(n);
        while at < 1000 && (seen as f64 / total as f64) >= at as f64 / 1000.0 {
            out.push(d);
            at += 1;
        }
    }
    out
}

fn pick(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

// --- plumbing -------------------------------------------------------------

struct Args {
    db: std::path::PathBuf,
    out: std::path::PathBuf,
    level: u32,
    tiers: Vec<u32>,
    bbox: Option<[f64; 4]>,
    min_footprint: f64,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Args, Box<dyn std::error::Error>> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("anon/bake/ has two parents")
            .to_path_buf();
        let mut out = Args {
            // Both live under build/, because both are generated: `make clean`
            // has to be able to take them away with everything else.
            db: std::env::var("MINIMAP_DB")
                .map(Into::into)
                .unwrap_or_else(|_| root.join("duckdb/minimap.duckdb")),
            out: root.join("anon/anon-zones.bin"),
            level: LEVEL,
            tiers: TIERS.to_vec(),
            bbox: None,
            min_footprint: 0.0,
        };
        let mut args = args.peekable();
        while let Some(flag) = args.next() {
            let mut next =
                || -> Result<String, String> { args.next().ok_or(format!("{flag} needs a value")) };
            match flag.as_str() {
                "--db" => out.db = next()?.into(),
                "--out" => out.out = next()?.into(),
                "--level" => out.level = next()?.parse()?,
                "--min-footprint" => out.min_footprint = next()?.parse()?,
                "--k" => {
                    out.tiers = next()?
                        .split(',')
                        .map(str::trim)
                        .map(str::parse)
                        .collect::<Result<_, _>>()?;
                }
                "--bbox" => {
                    let mut v = [0.0; 4];
                    for slot in &mut v {
                        *slot = next()?.parse()?;
                    }
                    out.bbox = Some(v);
                }
                other => return Err(format!("unknown flag {other}").into()),
            }
        }
        // The upper bound keeps the Hilbert key inside 48 bits; the lower one is
        // because the density window is coarser than the grid by construction,
        // and a level below it would shift by a negative number of bits.
        if !(DENSITY_LEVEL..=24).contains(&out.level) {
            return Err(format!(
                "--level {} out of range, wants {DENSITY_LEVEL}..=24",
                out.level
            )
            .into());
        }
        if out.tiers.is_empty() || out.tiers.iter().any(|&k| k < 2) {
            return Err("--k wants at least one value, each >= 2".into());
        }
        // Sorted so `Index::tier(None)` and the log read the same way.
        out.tiers.sort_unstable();
        out.tiers.dedup();
        Ok(out)
    }

    /// Restrict to a lon/lat box, on the centroid, in projected metres.
    fn bbox_sql(&self) -> String {
        let Some([w, s, e, n]) = self.bbox else {
            return String::new();
        };
        let mx = |lon: f64| lon / 180.0 * fmt::WORLD;
        let my = |lat: f64| {
            let lat = lat.clamp(-fmt::LAT_LIMIT, fmt::LAT_LIMIT);
            lat.to_radians().tan().asinh() * fmt::WORLD / std::f64::consts::PI
        };
        format!(
            "\n           AND (min_x + max_x) / 2 BETWEEN {} AND {}
           AND (min_y + max_y) / 2 BETWEEN {} AND {}",
            mx(w),
            mx(e),
            my(s),
            my(n)
        )
    }

    /// Drop buildings whose ground area is below `--min-footprint` square metres:
    /// the sheds and barns that inflate a hamlet's building count without adding
    /// anyone who could be at the position.
    fn footprint_sql(&self) -> String {
        if self.min_footprint <= 0.0 {
            return String::new();
        }
        format!(
            "\n           AND {} >= {}",
            footprint_m2(),
            self.min_footprint
        )
    }
}

/// Read-only, and with the same memory budget reasoning as the pipeline: DuckDB
/// defaults to 80% of RAM, which is the wrong number when the group-by is
/// sharing the machine with a 700 MB vector of cells.
fn connect(args: &Args) -> Result<duckdb::Connection, Box<dyn std::error::Error>> {
    if !args.db.exists() {
        return Err(format!("{} does not exist", args.db.display()).into());
    }
    let con = duckdb::Connection::open_with_flags(
        &args.db,
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    con.execute_batch("SET preserve_insertion_order = false")?;
    if let Some(limit) = std::env::var("MINIMAP_MEMORY_LIMIT").ok().or_else(|| {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(format!("{}MiB", kb / 1024 / 3))
    }) {
        con.execute_batch(&format!("SET memory_limit = '{limit}'"))?;
    }
    Ok(con)
}

fn timed(msg: impl AsRef<str>, since: Instant) {
    println!(
        "  {}  ({:.1}s)",
        msg.as_ref(),
        since.elapsed().as_secs_f64()
    );
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}
