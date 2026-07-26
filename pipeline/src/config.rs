//! Every number and rule the pipeline is tuned by, in one place.
//!
//! The SQL below is built as strings rather than written out literally because
//! almost all of it is derived from `MAXZOOM`, `MIN_PIXELS` or the class tables:
//! changing one constant has to change every threshold that depends on it, and
//! a literal query is a copy waiting to go stale.

use std::path::PathBuf;

/// Repository root: where `data/`, `web/` and the generated artefacts live.
///
/// Taken from the crate's location so a `cargo run` from anywhere works, and
/// overridable so a shipped binary is not tied to the build tree.
pub fn root() -> PathBuf {
    if let Ok(dir) = std::env::var("MINIMAP_ROOT") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().expect("pipeline/ has a parent").to_path_buf()
}

pub fn data() -> PathBuf {
    root().join("data")
}

/// Populated by ./fetch-europe.sh.
pub fn countries() -> PathBuf {
    data().join("countries")
}

pub fn db() -> PathBuf {
    root().join("minimap.duckdb")
}

pub fn archive() -> PathBuf {
    root().join("minimap.pmtiles")
}

/// Every extract we know how to fetch. Note that Hauts-de-France has no single
/// Geofabrik extract of its own: it is the union of the two legacy regions
/// merged by the 2016 reform, so it is spelled as both halves.
pub const SOURCES: [(&str, &str); 4] = [
    ("picardie", "https://download.geofabrik.de/europe/france/picardie-latest.osm.pbf"),
    (
        "nord-pas-de-calais",
        "https://download.geofabrik.de/europe/france/nord-pas-de-calais-latest.osm.pbf",
    ),
    ("france", "https://download.geofabrik.de/europe/france-latest.osm.pbf"),
    ("europe", "https://download.geofabrik.de/europe-latest.osm.pbf"),
];

/// Picardie alone while developing -- one 133 MB extract, a minute not an hour.
pub const DEFAULT_REGIONS: [&str; 1] = ["picardie"];

/// Where a region's extract lives.
///
/// `download` puts SOURCES regions flat into data/; ./fetch-europe.sh puts
/// country extracts in data/countries/ under Geofabrik's own `-latest` naming.
/// A region can be reachable by either route -- `france` and `europe` are in
/// SOURCES *and* fetched by the script -- so look for the file that is actually
/// there before deciding which layout this region uses.
pub fn pbf_path(region: &str) -> PathBuf {
    let flat = data().join(format!("{region}.osm.pbf"));
    if flat.exists() {
        return flat;
    }
    let country = countries().join(format!("{region}-latest.osm.pbf"));
    if country.exists() {
        return country;
    }
    // Neither is downloaded yet, so name where `download` would put it.
    if SOURCES.iter().any(|(n, _)| *n == region) {
        flat
    } else {
        country
    }
}

/// Everything loadable right now: known sources plus downloaded countries.
pub fn available_regions() -> Vec<String> {
    let mut names: Vec<String> = SOURCES.iter().map(|(n, _)| n.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(countries()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(region) = name.strip_suffix("-latest.osm.pbf") {
                names.push(region.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

pub const MINZOOM: u8 = 6;
pub const MAXZOOM: u8 = 14;
pub const EXTENT: u32 = 4096; // MVT integer grid per tile
pub const BUFFER: u32 = 64; // tile-unit overlap, so wide lines survive tile seams

/// Also the draw order.
pub const LAYERS: [&str; 4] = ["landuse", "water", "roads", "buildings"];

/// Web Mercator (EPSG:3857): the plane spans [-WORLD, WORLD] on both axes.
pub const WORLD: f64 = 20037508.342789244;

/// Projected metres per CSS pixel at z0. This must match the viewer's tile size
/// (web/minimap.js draws 512px tiles), not the historical 256px convention --
/// using the 256px value here makes every size threshold twice as strict as
/// what actually reaches the screen, which silently discarded buildings ~4px
/// wide.
pub const MPP0: f64 = 2.0 * WORLD / 512.0;

/// An area must span this many CSS pixels to be worth storing.
pub const MIN_PIXELS: f64 = 3.0;

/// Smallest projected span worth keeping at MAXZOOM. Anything below this cannot
/// reach MIN_PIXELS on screen even at the deepest zoom we bake, so the extractor
/// skips it before paying for WKB. This is what keeps ~1.8M sub-pixel buildings
/// per region out of the database.
pub const MIN_SPAN: f64 = MIN_PIXELS * MPP0 / (1u32 << MAXZOOM) as f64;

// --- classification -------------------------------------------------------
// A feature's `cls` drives styling; its `minzoom` is the cheapest and most
// effective data-reduction lever we have, so it is chosen deliberately per class
// (for roads) or from the feature's size (for areas).

/// class, matching `highway` values, minzoom.
pub const ROAD_CLASSES: [(&str, &[&str], u8); 8] = [
    ("motorway", &["motorway", "motorway_link"], 6),
    ("trunk", &["trunk", "trunk_link"], 7),
    ("primary", &["primary", "primary_link"], 8),
    ("secondary", &["secondary", "secondary_link"], 9),
    ("tertiary", &["tertiary", "tertiary_link"], 10),
    ("residential", &["residential", "unclassified", "living_street"], 11),
    ("service", &["service", "track"], 12),
    ("path", &["footway", "path", "cycleway", "pedestrian", "steps", "bridleway"], 12),
];

fn sql_list(values: &[&str]) -> String {
    values.iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect::<Vec<_>>().join(", ")
}

pub fn road_class_sql(col: &str) -> String {
    let arms: Vec<String> = ROAD_CLASSES
        .iter()
        .map(|(cls, tags, _)| format!("WHEN {col} IN ({}) THEN '{cls}'", sql_list(tags)))
        .collect();
    format!("CASE {} ELSE 'other' END", arms.join(" "))
}

pub fn road_minzoom_sql(col: &str) -> String {
    let arms: Vec<String> =
        ROAD_CLASSES.iter().map(|(cls, _, mz)| format!("WHEN '{cls}' THEN {mz}")).collect();
    format!("CASE {col} {} ELSE {MAXZOOM} END", arms.join(" "))
}

/// Smallest zoom at which a feature covers MIN_PIXELS screen pixels.
///
/// `sqrt(area) >= MIN_PIXELS * MPP0 / 2^z`, so
/// `z >= log2(MIN_PIXELS * MPP0 / sqrt(area))`. ST_Area is in projected units
/// and MPP0/2^z is the projected size of a pixel at that zoom, so the
/// comparison is latitude-independent in screen terms.
pub fn area_minzoom_sql() -> String {
    format!(
        "GREATEST({MINZOOM}, CAST(CEIL(LOG2(
            {MIN_PIXELS} * {MPP0} / GREATEST(SQRT(ST_Area(geom)), 1e-6)
         )) AS INTEGER))"
    )
}

/// Polygon classes, checked in order: the first matching arm wins.
pub const POLY_CLASS_SQL: &str = r#"
CASE
  WHEN "natural" = 'water' OR water IS NOT NULL
       OR waterway IN ('riverbank', 'dock')
       OR landuse IN ('reservoir', 'basin')                     THEN 'water'
  WHEN building IS NOT NULL AND building <> 'no'                THEN 'building'
  WHEN "natural" IN ('wood', 'scrub', 'heath', 'grassland')
       OR landuse = 'forest'                                    THEN 'wood'
  WHEN landuse IN ('farmland', 'farmyard', 'meadow', 'orchard',
                   'vineyard', 'greenhouse_horticulture')       THEN 'farmland'
  WHEN leisure IN ('park', 'garden', 'golf_course', 'pitch')
       OR landuse IN ('grass', 'recreation_ground', 'village_green',
                      'allotments', 'cemetery')                 THEN 'park'
  WHEN landuse IN ('residential', 'commercial', 'retail',
                   'industrial', 'railway', 'quarry')           THEN 'urban'
END"#;

pub const POLY_LAYER_SQL: &str = r#"
CASE cls
  WHEN 'water' THEN 'water'
  WHEN 'building' THEN 'buildings'
  ELSE 'landuse'
END"#;

// --- spatial clustering key ----------------------------------------------
// `cell` is the Morton (Z-order) code of the feature's bbox centre, on the tile
// grid at MAXZOOM. Two properties make it useful:
//   * sorting by it stores spatially-near features near each other on disk;
//   * the cell at any coarser zoom z is just  cell >> (2 * (MAXZOOM - z)),
//     so one column serves every zoom as a work-partition key.
// It is computed in SQL, not while extracting, because the bbox it needs is
// already being computed here.
//
// Note this is a clustering/partitioning key, not a lookup index: the bake reads
// whole layers, and measurement showed reading features is only ~1.6% of bake
// time (the other 98% is ST_AsMVTGeom clipping and ST_AsMVT encoding).
pub const BIT_SPREAD_MACRO: &str = "
CREATE OR REPLACE MACRO bit_spread(v) AS (
    WITH a AS (SELECT (v & 65535)::BIGINT n),
         b AS (SELECT ((n | (n << 8)) & 16711935) n FROM a),
         c AS (SELECT ((n | (n << 4)) & 252645135) n FROM b),
         d AS (SELECT ((n | (n << 2)) & 858993459) n FROM c),
         e AS (SELECT ((n | (n << 1)) & 1431655765) n FROM d)
    SELECT n FROM e)";

/// Morton code of the bbox centre on the MAXZOOM tile grid.
pub fn cell_sql(span: f64) -> String {
    let tile_x = format!("CAST(FLOOR((((min_x + max_x) / 2) + {WORLD}) / {span}) AS BIGINT)");
    let tile_y = format!("CAST(FLOOR(({WORLD} - ((min_y + max_y) / 2)) / {span}) AS BIGINT)");
    format!("(bit_spread({tile_x}) | (bit_spread({tile_y}) << 1))")
}

/// The staging table both the extractor and the classification SQL agree on.
/// `natural` needs quoting everywhere it appears: it is a SQL keyword.
pub const RAW_DDL: &str = r#"
    "kind" VARCHAR, "osm_id" BIGINT, "name" VARCHAR, "highway" VARCHAR,
    "waterway" VARCHAR, "building" VARCHAR, "landuse" VARCHAR, "natural" VARCHAR,
    "leisure" VARCHAR, "water" VARCHAR, "wkb" BLOB"#;

pub fn log(msg: impl AsRef<str>) {
    println!("  {}", msg.as_ref());
}

pub fn timed(msg: impl AsRef<str>, since: std::time::Instant) {
    println!("  {}  ({:.1}s)", msg.as_ref(), since.elapsed().as_secs_f64());
}

/// Thousands separators, because every count here is in the millions.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

