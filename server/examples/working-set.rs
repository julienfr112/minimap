//! What a city costs the page cache, and what one viewer costs the wire.
//!
//! Sizing a box for this server is not a CPU question -- a warm tile leaves the
//! process in about a microsecond (`make perf`) -- it is a question of whether
//! the tiles people actually look at fit in RAM. Traffic is never spread over
//! the map: it lands on a few dozen city centres, and those centres are a tiny
//! fraction of a 29 GB archive. This measures that fraction.
//!
//!   cargo run --release -p minimap-server --example working-set -- pmtiles
//!   ... [city ...]                only these, by name
//!
//! It sums the *stored* length of every tile in a box around each centre, at
//! the rung the viewer would use there. Summing lengths reads directories and
//! never touches tile data, so this costs a few hundred page faults rather than
//! the gigabytes it is measuring.
//!
//! Two areas per city, because they are cached for different reasons:
//!
//!   * **z17, 12x12 km** -- the deepest rung, and where someone panning around
//!     a centre actually spends their time. This is the number that has to stay
//!     resident for the map to feel instant.
//!   * **z15, 48x48 km** -- the metropolitan view, one zoom step out. Four times
//!     the ground per tile, so it is cheap and it covers every approach to the
//!     city.
//!
//! Everything shallower (z12 and z10, which is also where `land` and `landuse`
//! stop) is measured once for the whole archive: it is shared by every user
//! everywhere, so it is a fixed cost, not a per-city one.

use std::{collections::BTreeMap, path::PathBuf};

use minimap_server::pmtiles::Archive;

/// The rungs this map is baked at, and the two a city view uses. Kept here
/// rather than read from the archives so the report is comparable across
/// builds; `meta.json` is the authority if they ever disagree.
const CITY_RUNG: u8 = 17;
const METRO_RUNG: u8 = 15;

/// Half-widths, in km. A viewport is ~2 km across at z17, so 6 km each way is
/// a good hour of panning; 24 km at z15 is the city and its ring road.
const CITY_KM: f64 = 6.0;
const METRO_KM: f64 = 24.0;

/// A 1400x900 viewport, in tiles, at the zoom where a rung is drawn 1:1 --
/// which is the worst case, since one step further out doubles the ground each
/// tile covers.
const SCREEN_TILES: usize = 7 * 5;

/// Somewhere central in each city, and the point is the density, not the
/// landmark. Cities outside the archive's bounds are skipped with a note.
const CITIES: [(&str, f64, f64); 24] = [
    ("Paris", 48.857, 2.352),
    ("London", 51.507, -0.128),
    ("Berlin", 52.520, 13.405),
    ("Madrid", 40.417, -3.704),
    ("Rome", 41.903, 12.496),
    ("Milan", 45.464, 9.190),
    ("Barcelona", 41.385, 2.173),
    ("Munich", 48.135, 11.582),
    ("Hamburg", 53.551, 9.994),
    ("Vienna", 48.209, 16.373),
    ("Warsaw", 52.230, 21.012),
    ("Budapest", 47.498, 19.040),
    ("Bucharest", 44.427, 26.103),
    ("Prague", 50.076, 14.438),
    ("Amsterdam", 52.370, 4.895),
    ("Brussels", 50.851, 4.352),
    ("Lisbon", 38.722, -9.139),
    ("Athens", 37.984, 23.728),
    ("Stockholm", 59.329, 18.069),
    ("Copenhagen", 55.677, 12.568),
    ("Dublin", 53.350, -6.260),
    ("Naples", 40.852, 14.268),
    ("Lyon", 45.764, 4.836),
    ("Marseille", 43.296, 5.370),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().unwrap_or_else(|| "pmtiles".into()));
    let only: Vec<String> = args.map(|a| a.to_lowercase()).collect();

    // One archive per layer, in whatever the build produced.
    let mut layers: BTreeMap<String, Archive> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e} -- run `make all` first", dir.display()))?
        .flatten()
    {
        let file = entry.file_name().to_string_lossy().into_owned();
        if let Some(name) = file.strip_suffix(".pmtiles") {
            layers.insert(name.to_string(), Archive::open(&entry.path())?);
        }
    }
    if layers.is_empty() {
        return Err(format!("no .pmtiles in {}", dir.display()).into());
    }

    let total: u64 = layers
        .values()
        .map(|a| a.tile_count)
        .sum();
    println!(
        "{} layers, {} tiles: {}",
        layers.len(),
        thousands(total),
        layers
            .iter()
            .map(|(n, a)| format!("{n} z{}..{}", a.min_zoom, a.max_zoom))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // The shallow rungs are one working set for everybody: whoever loads the
    // map anywhere pulls them in, and they never leave.
    let mut shared = 0u64;
    for (_, a) in layers.iter() {
        for z in a.min_zoom..=12.min(a.max_zoom) {
            shared += whole_rung(a, z);
        }
    }
    println!(
        "shared by every user (every rung up to z12): {}\n",
        bytes(shared)
    );

    println!(
        "{:<12} {:>12} {:>12} {:>12}   {}",
        "city",
        format!("z{CITY_RUNG} {}x{} km", CITY_KM * 2.0, CITY_KM * 2.0),
        format!("z{METRO_RUNG} {}x{} km", METRO_KM * 2.0, METRO_KM * 2.0),
        "both",
        "first screenful at z17"
    );

    let mut running = shared;
    let mut counted = 0usize;
    for (name, lat, lon) in CITIES {
        if !only.is_empty() && !only.contains(&name.to_lowercase()) {
            continue;
        }
        // A centre outside the build is not a gap in the report, it is a
        // country that was never downloaded.
        if !layers.values().any(|a| {
            lon >= a.min_lon && lon <= a.max_lon && lat >= a.min_lat && lat <= a.max_lat
        }) {
            println!("{name:<12} outside the archive's bounds -- not in this build");
            continue;
        }

        let city: u64 = layers
            .values()
            .map(|a| box_bytes(a, lat, lon, CITY_KM, CITY_RUNG))
            .sum();
        let metro: u64 = layers
            .values()
            .map(|a| box_bytes(a, lat, lon, METRO_KM, METRO_RUNG))
            .sum();
        // What a browser pulls when someone opens the map here cold: one
        // screenful of every layer that reaches this rung, plus the background
        // rungs it already shares with everyone.
        let screen: u64 = layers
            .values()
            .map(|a| screenful(a, lat, lon, CITY_RUNG))
            .sum();

        running += city + metro;
        counted += 1;
        println!(
            "{name:<12} {:>12} {:>12} {:>12}   {}",
            bytes(city),
            bytes(metro),
            bytes(city + metro),
            bytes(screen),
        );
    }

    println!(
        "\n{counted} cities plus the shared rungs: {} resident once every centre is warm",
        bytes(running)
    );
    println!(
        "  that is the page cache this needs. What is left of RAM after it is what\n  \
         the process itself uses, which `make perf` measures at ~0 bytes per request."
    );
    println!(
        "  a viewer costs {SCREEN_TILES} tiles per layer per screenful, and nothing at all\n  \
         once idle -- there is no session, no polling and no socket to hold open."
    );
    Ok(())
}

/// Stored bytes of every tile of `archive` inside a box of `half_km` around
/// (`lat`, `lon`) at `rung`. Zero when the layer does not reach that rung,
/// which is how `land` and `landuse` drop out of the deep numbers by
/// themselves.
fn box_bytes(archive: &Archive, lat: f64, lon: f64, half_km: f64, rung: u8) -> u64 {
    if rung < archive.min_zoom || rung > archive.max_zoom {
        return 0;
    }
    let (x0, y0, x1, y1) = tile_box(lat, lon, half_km, rung);
    let mut total = 0u64;
    for x in x0..=x1 {
        for y in y0..=y1 {
            total += archive.tile(rung, x, y).map_or(0, |t| t.len() as u64);
        }
    }
    total
}

/// One viewport's worth of tiles at `rung`, centred on the position.
fn screenful(archive: &Archive, lat: f64, lon: f64, rung: u8) -> u64 {
    if rung < archive.min_zoom || rung > archive.max_zoom {
        // The layer is still drawn -- from whatever rung it does reach -- but
        // those tiles are the shared shallow ones, already counted.
        return 0;
    }
    let (cx, cy) = to_tile(lat, lon, rung);
    let side = (SCREEN_TILES as f64).sqrt().ceil() as u32;
    let mut total = 0u64;
    for x in cx.saturating_sub(side / 2)..=cx + side / 2 {
        for y in cy.saturating_sub(side / 2)..=cy + side / 2 {
            total += archive.tile(rung, x, y).map_or(0, |t| t.len() as u64);
        }
    }
    total
}

/// Every tile of one rung. Only used for the shallow rungs, where a whole level
/// is a few thousand tiles.
fn whole_rung(archive: &Archive, z: u8) -> u64 {
    let n = 1u32 << z;
    let (x0, y0) = to_tile(archive.max_lat, archive.min_lon, z);
    let (x1, y1) = to_tile(archive.min_lat, archive.max_lon, z);
    let mut total = 0u64;
    for x in x0..=x1.min(n - 1) {
        for y in y0..=y1.min(n - 1) {
            total += archive.tile(z, x, y).map_or(0, |t| t.len() as u64);
        }
    }
    total
}

/// The tile range covering a box of `half_km` each way around a position.
fn tile_box(lat: f64, lon: f64, half_km: f64, z: u8) -> (u32, u32, u32, u32) {
    // Mercator: a tile is this many metres of ground, narrowing with latitude.
    let per_tile = 40_075_016.686 * lat.to_radians().cos() / f64::from(1u32 << z);
    let span = (half_km * 1000.0 / per_tile).ceil() as u32;
    let (cx, cy) = to_tile(lat, lon, z);
    let n = 1u32 << z;
    (
        cx.saturating_sub(span),
        cy.saturating_sub(span),
        (cx + span).min(n - 1),
        (cy + span).min(n - 1),
    )
}

fn to_tile(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0);
    let rad = lat.to_radians();
    let y = ((1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .clamp(0.0, n - 1.0);
    (x as u32, y as u32)
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.2} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} kB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}
