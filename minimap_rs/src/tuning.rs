//! Every number and rule the *map* is tuned by, in one place.
//!
//! Where the map is written to and how deep it is baked are a run's business
//! and live in [`crate::config::Config`]; what a road is and when a building
//! becomes worth drawing are the map's, and live here.
//!
//! The SQL below is built as strings rather than written out literally because
//! almost all of it is derived from the zoom range, [`MIN_PIXELS`] or the class
//! tables: changing one input has to change every threshold that depends on it,
//! and a literal query is a copy waiting to go stale.
//!
//! The zoom rungs and the size thresholds live here rather than being flags,
//! because they are not settings — they are what this map is. A different set
//! is a different map, and the Makefile depends on this file, so editing it
//! re-runs exactly the stages that went stale.

/// The zoom levels baked — "rungs". Not a range: the viewer reuses a shallower
/// tile drawn larger for the levels in between, which holds up to about 4x
/// before the geometry, simplified to one pixel at *its* zoom, reads as blurry.
///
/// These four cover z10 (49 km across) to z18 (192 m) at integer steps, and put
/// their only 4x stretch on z14 — between the two views this map is for, rather
/// than inside either. Each rung is ~4x the tiles of the one above it, so the
/// deepest is essentially the whole cost of a build.
pub const ZOOMS: [u8; 4] = [10, 12, 15, 17];

/// The rungs as `10,12,15,17`, for banners and for the archive metadata.
pub fn zooms_csv() -> String {
    ZOOMS.iter().map(|z| z.to_string()).collect::<Vec<_>>().join(",")
}

pub fn minzoom() -> u8 {
    ZOOMS[0]
}

pub fn maxzoom() -> u8 {
    ZOOMS[ZOOMS.len() - 1]
}

/// The deepest rung carrying the areal background; see [`is_background`].
///
/// `land` and `landuse` tile everything, so at a deep rung they force full
/// coverage — z17 over Picardie is 1.04M tiles, almost all of them saying
/// "still farmland". Buildings and roads exist only where people do. Capping the
/// background here and letting the viewer keep this rung's tiles underneath is
/// what makes a deep rung affordable at all.
pub const BACKGROUND_MAXZOOM: u8 = 12;

/// Whether `layer` is baked at rung `z`. The background cap is the only thing
/// that varies; every other layer is baked at every rung, and the per-feature
/// `minzoom` decides the rest.
pub fn bakes(layer: &str, z: u8) -> bool {
    !is_background(layer) || z <= BACKGROUND_MAXZOOM
}

/// The deepest rung carrying background — what the viewer falls back to for
/// land and landuse once it is past the cap.
pub fn background_rung() -> u8 {
    let mut best = minzoom();
    for z in ZOOMS {
        if z <= BACKGROUND_MAXZOOM {
            best = z;
        }
    }
    best
}

pub const EXTENT: u32 = 4096; // MVT integer grid per tile
pub const BUFFER: u32 = 64; // tile-unit overlap, so wide lines survive tile seams

/// Also the draw order. `places` is last because labels go over everything.
pub const LAYERS: [&str; 6] = ["land", "landuse", "water", "roads", "buildings", "places"];

/// The one layer whose tiles carry a `name`. Names are otherwise left in
/// `features` -- putting them on every feature inflates a tile by roughly 40%
/// to write text nothing draws. A label is the exception: the name *is* the
/// feature.
pub const NAMED_LAYER: &str = "places";

/// Web Mercator (EPSG:3857): the plane spans [-WORLD, WORLD] on both axes.
pub const WORLD: f64 = 20037508.342789244;

/// Projected metres per CSS pixel at z0. This must match the viewer's tile size
/// (web/minimap.js draws 512px tiles), not the historical 256px convention --
/// using the 256px value here makes every size threshold twice as strict as
/// what actually reaches the screen, which silently discarded buildings ~4px
/// wide.
pub const MPP0: f64 = 2.0 * WORLD / 512.0;

/// How many CSS pixels a feature must span to be worth drawing.
///
/// 3 px is the honest visibility threshold: below it a polygon is a speck. It is
/// also what the extractor keeps, so raising it discards data — at 12 the deep
/// rung loses 416,781 buildings, a quarter of them, all the ones under 4.6 m.
pub const MIN_PIXELS: f64 = 3.0;

/// The same threshold for the `landuse` layer alone.
///
/// Higher, because the honest visibility floor is the wrong number for texture.
/// A farmland field three pixels across is noise; a building three pixels across
/// is the thing you were looking for. At 12 the wide view carries 4,902 farmland
/// polygons instead of 31,089, and not one building is lost.
pub const LANDUSE_PIXELS: f64 = 12.0;

/// Smallest projected span worth keeping at `maxzoom`. Anything below this
/// cannot reach [`MIN_PIXELS`] on screen even at the deepest zoom being baked,
/// so the extractor skips it before paying for WKB. This is what keeps ~1.8M
/// sub-pixel buildings per region out of the database.
///
/// It is also why a database is specific to the maxzoom it was loaded for, and
/// why the Makefile puts that number in the stamp filename.
pub fn min_span() -> f64 {
    MIN_PIXELS * MPP0 / (1u32 << maxzoom()) as f64
}

/// Side of one tile at zoom `z`, in projected units.
pub fn tile_span(z: u8) -> f64 {
    2.0 * WORLD / (1u32 << z) as f64
}

/// The areal background: the layers that cover the whole map rather than the
/// places something happens to be.
///
/// This is the distinction that decides what a deep rung costs. `land` and
/// `landuse` tile everything, so baking them at z17 means one tile per 197 m of
/// countryside whether or not anything is there. `buildings` and `roads` are
/// sparse -- they exist where people do -- so a deep rung carrying only those
/// covers towns and skips the fields between them.
pub fn is_background(layer: &str) -> bool {
    matches!(layer, "land" | "landuse")
}

/// Layers made of areas. They get the topology-preserving simplifier and a
/// validity repair after it; lines cannot be simplified into an invalid ring, so
/// they skip both.
pub fn is_area(layer: &str) -> bool {
    matches!(layer, "land" | "landuse" | "water" | "buildings")
}

/// The ocean is not in OpenStreetMap. Coastlines are tagged on ways
/// (`natural=coastline`) and assembling a planet's worth of them into polygons
/// is its own program -- OSMCoastline -- whose output osmdata.openstreetmap.de
/// publishes. So the sea has to come from here or not at all.
///
/// Take the *land* side rather than the water side. With a sea-coloured
/// background, open ocean then costs nothing at all: no polygon, no tile, no
/// byte. Water polygons over a land background would instead mean emitting a
/// blue rectangle for every sea tile in the bounding box, and 65% of Europe's
/// box is sea -- some eleven million tiles at z14 to say "still the Atlantic".
pub const LAND_URL: &str = "https://osmdata.openstreetmap.de/download/land-polygons-split-3857.zip";

/// Geofabrik's machine-readable list of everything it publishes. Regions are
/// resolved through it rather than against a hardcoded table, so a name that
/// works on their download page works here.
pub const GEOFABRIK_INDEX: &str = "https://download.geofabrik.de/index-v1-nogeom.json";

/// Aggregates that overlap their own siblings. Taking them as well would
/// download gigabytes twice and double-count features at load time:
///   alps / dach          span several countries
///   britain-and-ireland  = great-britain + ireland-and-northern-ireland
///   united-kingdom       overlaps great-britain and northern ireland
pub const EUROPE_SKIP: [&str; 4] = ["alps", "dach", "britain-and-ireland", "united-kingdom"];

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
    ("tertiary", &["tertiary", "tertiary_link"], 11),
    (
        "residential",
        &["residential", "unclassified", "living_street"],
        11,
    ),
    ("service", &["service", "track"], 12),
    (
        "path",
        &[
            "footway",
            "path",
            "cycleway",
            "pedestrian",
            "steps",
            "bridleway",
        ],
        12,
    ),
];

fn sql_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("'{}'", v.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn road_class_sql(col: &str) -> String {
    let arms: Vec<String> = ROAD_CLASSES
        .iter()
        .map(|(cls, tags, _)| format!("WHEN {col} IN ({}) THEN '{cls}'", sql_list(tags)))
        .collect();
    format!("CASE {} ELSE 'other' END", arms.join(" "))
}

/// Road minzooms are absolute (a motorway is worth drawing at z6 whatever else
/// is being baked), but clamped into the range actually being built: baking
/// only to z10 must not silently drop every service road, and baking a range
/// starting at z8 must not ask for tiles at z6 that will never exist.
pub fn road_minzoom_sql(col: &str) -> String {
    let arms: Vec<String> = ROAD_CLASSES
        .iter()
        .map(|(cls, _, mz)| {
            format!("WHEN '{cls}' THEN {}", (*mz).clamp(minzoom(), maxzoom()))
        })
        .collect();
    format!("CASE {col} {} ELSE {} END", arms.join(" "), maxzoom())
}

/// Which zoom a settlement earns a label at, by how many people live there.
///
/// Tuned to the viewer's rungs rather than to a smooth ramp: on the widest one
/// only capitals and big cities should survive, or the map is a wall of text.
/// Population is missing often enough that `place=city` alone has to be worth
/// something.
pub fn place_minzoom_sql(pop: &str, kind: &str) -> String {
    let z = |want: u8| want.clamp(minzoom(), maxzoom());
    format!(
        "CASE WHEN {pop} >= 200000 THEN {}
              WHEN {pop} >= 50000 OR {kind} = 'city' THEN {}
              ELSE {} END",
        z(10),
        z(12),
        z(14)
    )
}

/// Smallest zoom at which a feature covers [`MIN_PIXELS`] screen pixels.
///
/// `sqrt(area) >= MIN_PIXELS * MPP0 / 2^z`, so
/// `z >= log2(MIN_PIXELS * MPP0 / sqrt(area))`. ST_Area is in projected units
/// and MPP0/2^z is the projected size of a pixel at that zoom, so the
/// comparison is latitude-independent in screen terms.
pub fn area_minzoom_sql() -> String {
    format!(
        "GREATEST({}, CAST(CEIL(LOG2(
            (CASE WHEN cls IN ({texture}) THEN {LANDUSE_PIXELS} ELSE {MIN_PIXELS} END)
            * {MPP0} / GREATEST(SQRT(ST_Area(geom)), 1e-6)
         )) AS INTEGER))",
        minzoom(),
        texture = sql_list(&LANDUSE_CLASSES)
    )
}

/// The clustering key, on the tile grid at the deepest rung.
pub fn cell_sql_at_maxzoom() -> String {
    cell_sql(tile_span(maxzoom()))
}

/// The classes that make up the `landuse` layer -- see [`POLY_LAYER_SQL`].
///
/// They get their own size threshold because the honest visibility floor is the
/// wrong one for them. A farmland field three pixels across is texture; a
/// building three pixels across is the thing you were looking for. Applying one
/// number to both means either a wide view drowning in fields or a building view
/// missing a quarter of its buildings -- measured, 416,781 of them on Picardie.
pub const LANDUSE_CLASSES: [&str; 4] = ["wood", "farmland", "park", "urban"];

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
// grid at maxzoom. Two properties make it useful:
//   * sorting by it stores spatially-near features near each other on disk;
//   * the cell at any coarser zoom z is just  cell >> (2 * (maxzoom - z)),
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

/// Morton code of the bbox centre on the tile grid of side `span`.
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
