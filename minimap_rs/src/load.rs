//! PBF -> DuckDB `features`.
//!
//! The extractor appends drawable objects as WKB straight into a `raw` table --
//! no staging file, no serialisation. One SQL pass then classifies them into a
//! layer and a class, reprojects to EPSG:3857, caches each bounding box (the
//! bake needs those to assign features to tiles) and attaches a spatial `cell`
//! key.
//!
//! **`raw` is per region, not per run.** It is pure scaffolding, and it is
//! enormous: on France it was 53.7 GB of a 78.1 GB database, against 19.8 GB of
//! actual features. Staging every region before classifying any would make the
//! continent impossible on any disk one is likely to have -- Europe would need
//! ~360 GB of `raw` alone. Staging, classifying and dropping one region at a
//! time bounds it to the largest single country instead, so peak disk becomes
//! `all features + one France` rather than `all features + all of Europe`. The
//! CHECKPOINT is what makes the freed blocks reusable rather than merely free.
//!
//! **Nothing here rewrites the whole table.** Deduplicating with
//! `CREATE OR REPLACE TABLE features AS SELECT ... FROM features` has to
//! materialise the new copy while the old one still exists, so it needs the
//! table's size over again in free space -- another 103 GB at Europe scale, and
//! it falls due at the very end of a fifty-minute load. So the size filter and
//! the spatial `cell` are applied per region on the way in, and duplicates are
//! deleted in place at the end. The cost is that features are cell-ordered only
//! within a region rather than globally, which is most of the benefit anyway:
//! regions are geographically disjoint, so a per-region sort already puts
//! neighbours near neighbours.
//!
//! Note the division of labour: the extractor decided *whether* an object is
//! drawable, and everything about *what it is* is decided here, in SQL, in one
//! pass over a columnar table. Doing the classification per object during
//! extraction would be a branch per row in the one place that is already the
//! bottleneck.

use std::time::Instant;

use duckdb::Connection;

use crate::config::{Config, Region};
use crate::extract;
use crate::progress::{self, Step};
use crate::rows::RawTable;
use crate::tuning::{self, WORLD};

type Error = Box<dyn std::error::Error>;

pub fn run(cfg: &Config, con: &Connection, regions: &[Region]) -> Result<(), Error> {
    if !cfg.land_zip().exists() {
        return Err(format!(
            "{} is missing -- run `make download` (the coastline is a separate dataset)",
            cfg.land_zip().display()
        )
        .into());
    }

    let step = Step::start(
        "load",
        format!(
            "{} extract{} -> {}  (for rungs z{})",
            regions.len(),
            if regions.len() == 1 { "" } else { "s" },
            cfg.db().display(),
            tuning::zooms_csv()
        ),
    );

    // A region costs roughly its own size in bytes, which is the one thing
    // known before any work happens, so that is the unit the bar counts in.
    // The coastline tail is not a region and does not scale with them -- it
    // scales with the bounding box -- so it is modelled as a fixed amount of
    // equivalent bytes plus a slice of the total. Measured on Picardie the tail
    // was ~40% of a one-region load and it is a few percent of Europe's; this
    // splits the difference and errs towards over-counting, so the bar slows
    // down at the end rather than sitting at 100% while work continues.
    let sizes: Vec<f64> = regions
        .iter()
        .map(|r| std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0) as f64)
        .collect();
    let bytes: f64 = sizes.iter().sum();
    let tail = 0.15 * bytes + 80e6;
    progress::begin(bytes + tail);

    con.execute_batch(
        "DROP TABLE IF EXISTS features;
         CREATE TABLE features (
            layer   VARCHAR,
            cls     VARCHAR,
            minzoom UTINYINT,
            name    VARCHAR,
            osm_id  BIGINT,
            geom    GEOMETRY,
            min_x   DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE,
            cell    BIGINT
         )",
    )?;
    // Used by every per-region insert below to compute `cell`.
    con.execute_batch(tuning::BIT_SPREAD_MACRO)?;

    for (i, region) in regions.iter().enumerate() {
        let prefix = progress::item(i, regions.len(), &region.name);

        // Measured 8.7s parsing against 4.0s classifying on Picardie, so the
        // region's weight splits about two to one between them.
        let (parse, classify) = (sizes[i] * 0.65, sizes[i] * 0.35);

        let t0 = Instant::now();
        con.execute_batch(&format!(
            "DROP TABLE IF EXISTS raw; CREATE TABLE raw ({})",
            tuning::RAW_DDL
        ))?;
        let mut sink = RawTable::new(con.appender("raw")?);
        extract::run(&region.path, &mut sink, tuning::min_span(), &region.name, parse)?;
        let n = sink.finish()?;
        progress::timed(
            format!("{prefix} staged {} objects", progress::commas(n)),
            t0,
        );

        let t0 = Instant::now();
        progress::at(format!("{} classifying lines", region.name));
        classify_lines(con)?;
        progress::at(format!("{} classifying areas", region.name));
        classify_areas(con)?;
        progress::tick(classify);
        con.execute_batch("DROP TABLE raw; CHECKPOINT")?;
        let total: i64 = con.query_row("SELECT count(*) FROM features", [], |r| r.get(0))?;
        progress::timed(
            format!(
                "{prefix} classified, {} features so far",
                progress::commas(total as u64)
            ),
            t0,
        );
    }

    // Adjacent extracts overlap along their shared border, so the same way can
    // arrive twice. Deleted in place -- see the note at the top of this file for
    // why this is not a rewrite.
    let t0 = Instant::now();
    progress::at("removing cross-border duplicates");
    let before: i64 = con.query_row("SELECT count(*) FROM features", [], |r| r.get(0))?;
    con.execute_batch(
        "DELETE FROM features WHERE rowid IN (
             SELECT rowid FROM (
                 SELECT rowid, row_number() OVER (
                            PARTITION BY layer, osm_id ORDER BY rowid) AS rn
                 FROM features WHERE osm_id IS NOT NULL
             ) WHERE rn > 1
         );
         CHECKPOINT",
    )?;
    let after: i64 = con.query_row("SELECT count(*) FROM features", [], |r| r.get(0))?;
    progress::timed(
        format!(
            "{} -> {} features, duplicates removed",
            progress::commas(before as u64),
            progress::commas(after as u64)
        ),
        t0,
    );

    // The tail, spent in the steps that are not per-region.
    land(cfg, con, tail * 0.45)?;
    subdivide(con, "land", tail * 0.20)?;
    subdivide(con, "water", tail * 0.15)?;
    places(con, regions, tail * 0.20)?;

    // The work is over, so the bar comes down before the summary rather than
    // having a deferred redraw land in the middle of it.
    progress::end();
    con.execute_batch("CHECKPOINT")?;

    let mut stmt = con.prepare(
        "SELECT layer, count(*) n, min(minzoom), max(minzoom)
         FROM features GROUP BY layer ORDER BY n DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    println!();
    for row in rows {
        let (layer, n, lo, hi) = row?;
        progress::line(format!(
            "{layer:<12} {:>12}  minzoom {lo}..{hi}",
            progress::commas(n as u64)
        ));
    }
    step.done();
    Ok(())
}

/// Settlement labels, from `place` nodes.
///
/// A separate scan of the extracts (see `extract::scan_places`) because nothing
/// else in the pipeline treats a node as a feature. The result is small enough
/// -- tens of thousands for a continent -- to come back as a Vec and go in
/// through one appender, so this needs none of the `raw` staging the ways use.
pub fn places(con: &Connection, regions: &[Region], weight: f64) -> Result<(), Error> {
    let t0 = Instant::now();
    progress::at("scanning for place labels");
    con.execute_batch(
        "CREATE OR REPLACE TABLE raw_places
             (name VARCHAR, kind VARCHAR, population BIGINT, lon DOUBLE, lat DOUBLE)",
    )?;
    let mut total = 0usize;
    for region in regions {
        let found = extract::scan_places(&region.path)?;
        let mut appender = con.appender("raw_places")?;
        for p in &found {
            appender.append_row(duckdb::params![p.name, p.kind, p.population, p.lon, p.lat])?;
        }
        appender.flush()?;
        total += found.len();
    }
    // Adjacent extracts overlap, so a border city arrives more than once. Same
    // name at the same spot to within a maxzoom tile is the same city: `osm_id`
    // is not available here to dedup on, and would not help across the two nodes
    // OSM sometimes has for one place anyway.
    con.execute_batch(&format!(
        r#"
        INSERT INTO features (layer, cls, minzoom, name, osm_id, geom,
                              min_x, min_y, max_x, max_y, cell)
        WITH deduped AS (
            SELECT name, kind, max(population) AS population,
                   min(lon) AS lon, min(lat) AS lat
            FROM raw_places
            GROUP BY name, kind, round(lon, 3), round(lat, 3)
        ), projected AS (
            SELECT name, kind, population,
                   ST_Transform(ST_Point(lon, lat), 'EPSG:4326', 'EPSG:3857',
                                always_xy := true) AS geom
            FROM deduped
        )
        SELECT 'places', kind, {minzoom}, name, NULL, geom,
               ST_X(geom), ST_Y(geom), ST_X(geom), ST_Y(geom), {cell} AS cell
        FROM (SELECT *, ST_X(geom) AS min_x, ST_Y(geom) AS min_y,
                        ST_X(geom) AS max_x, ST_Y(geom) AS max_y
              FROM projected)
        ORDER BY cell
        "#,
        minzoom = tuning::place_minzoom_sql("population", "kind"),
        cell = tuning::cell_sql_at_maxzoom(),
    ))?;
    con.execute_batch("DROP TABLE raw_places")?;
    let kept: i64 = con.query_row(
        "SELECT count(*) FROM features WHERE layer = 'places'",
        [],
        |r| r.get(0),
    )?;
    progress::timed(
        format!(
            "{} place nodes scanned, {} labels kept",
            progress::commas(total as u64),
            progress::commas(kept as u64)
        ),
        t0,
    );
    progress::tick(weight);
    Ok(())
}

/// Cut an areal layer to a coarse grid before anything tries to bake it.
///
/// `ST_AsMVTGeom` clips the *whole* geometry for every tile it touches, so a
/// feature costs roughly (vertices x tiles covered). That product is harmless
/// for OSM's median -- a building has a handful of vertices and touches one
/// tile -- and ruinous for anything both large and detailed. The coastline was
/// the first casualty: p99 is 595 vertices, but the largest chunk is 174,320
/// spread over a bounding box of 16,394 z14 tiles, some 1.4 billion
/// vertex-clips for one row. Measured before this existed, z14 land ran at
/// 51.7 ms per tile to emit 92 bytes, and the zoom was heading for eleven
/// hours. Water has the same monsters -- riverbank relations and big lakes run
/// to hundreds of thousands of vertices over bounding boxes of thousands of
/// deep-rung tiles -- it just wasn't baked deep enough to hurt until z15.
///
/// Cutting each feature to a cell bounds the product, because a piece can then
/// only reach the tiles of its own cell. maxzoom-3 is an 8x8-tile cell: fine
/// enough to bound the cost, coarse enough that splitting stays cheap and the
/// extra pieces stay in the hundreds of thousands. Features already inside
/// a single cell -- almost all of them -- skip the intersection entirely.
///
/// The cutting is done by *bisection* -- split anything wider than a cell in
/// half along its wider axis, snapped to the cell grid, and repeat -- rather
/// than by exploding each feature against every cell of its bounding box in
/// one statement. The one-shot explode duplicates the whole parent geometry
/// once per cell row inside the pipeline, and Europe's water is where that
/// stops being a detail: a riverbank multipolygon of a few hundred thousand
/// vertices crossed with the hundreds of cells its box spans is gigabytes in
/// flight, and the water pass blew DuckDB's memory budget exactly there
/// (land survived only because coastline chunks arrive pre-split). Halving
/// keeps at most two copies of any geometry in flight, and the copies shrink
/// as the passes go: O(V log cells) work instead of O(V x cells).
///
/// Pieces keep their parent's `cls` and `minzoom`: a sliver cut off a lake that
/// earned z10 by its full size must still appear at z10, or the lake grows
/// holes on the wide view.
///
/// (The other suspect was DuckDB re-evaluating the simplify per exploded row.
/// Forcing the CTEs to materialise measured 370s against 344s, so no: it was
/// already hoisting them.)
pub fn subdivide(con: &Connection, layer: &str, weight: f64) -> Result<(), Error> {
    let marker: i64 = con.query_row(
        &format!(
            "SELECT count(*) FROM duckdb_tables() WHERE table_name = '{layer}_subdivided'"
        ),
        [],
        |r| r.get(0),
    )?;
    if marker > 0 {
        progress::line(format!("{layer} already subdivided"));
        progress::tick(weight);
        return Ok(());
    }
    let t0 = Instant::now();
    progress::at(format!("subdividing {layer}"));
    // `cell` needs the macro; created here as well as in `run` so a bake can
    // subdivide a database loaded before its layer got this treatment.
    con.execute_batch(tuning::BIT_SPREAD_MACRO)?;
    let cell_zoom = tuning::maxzoom().saturating_sub(3).max(tuning::minzoom());
    let cell_span = tuning::tile_span(cell_zoom);
    con.execute_batch(&format!(
        "CREATE OR REPLACE TABLE layer_sub AS
         SELECT cls, minzoom, name, osm_id, geom, min_x, min_y, max_x, max_y
         FROM features WHERE layer = '{layer}'"
    ))?;
    // Cell indices from the cached bbox, recomputed each pass. Each bound leans
    // inward by a billionth of a cell (a couple of microns), because every cut
    // puts edges exactly on a cell boundary and exact is not a thing floats do:
    // an edge one ulp on the wrong side of its boundary would read as
    // straddling, and a piece that straddles by nothing splits into an empty
    // sliver and itself, forever.
    let cx0 = format!("FLOOR((min_x + {WORLD}) / {cell_span} + 1e-9)");
    let cx1 = format!("FLOOR((max_x + {WORLD}) / {cell_span} - 1e-9)");
    let cy0 = format!("FLOOR(({WORLD} - max_y) / {cell_span} + 1e-9)");
    let cy1 = format!("FLOOR(({WORLD} - min_y) / {cell_span} - 1e-9)");
    let wide = format!("({cx1} > {cx0} OR {cy1} > {cy0})");
    // Fewer threads while splitting: every thread holds vectors of geometry
    // copies, and the whole point of this loop is staying inside the memory
    // budget. The cap on passes cannot bind -- halving reaches a single cell
    // from the whole world in 14 -- it is a backstop against looping forever
    // on a geometry that somehow refuses to shrink.
    con.execute_batch("SET threads TO 4")?;
    for _pass in 0..64 {
        let n: i64 = con.query_row(
            &format!("SELECT count(*) FROM layer_sub WHERE {wide}"),
            [],
            |r| r.get(0),
        )?;
        if n == 0 {
            break;
        }
        progress::at(format!("subdividing {layer}: {} pieces still oversized", progress::commas(n as u64)));
        con.execute_batch(&format!(
            r#"
            CREATE OR REPLACE TABLE split_src AS
            SELECT cls, minzoom, name, osm_id, geom, min_x, min_y, max_x, max_y
            FROM layer_sub WHERE {wide};
            DELETE FROM layer_sub WHERE {wide};
            INSERT INTO layer_sub
            WITH m AS (
                SELECT cls, minzoom, name, osm_id, geom, min_x, min_y, max_x, max_y,
                       ({cx1} - {cx0}) >= ({cy1} - {cy0}) AS split_x,
                       {} + (FLOOR(({cx0} + {cx1}) / 2) + 1) * {cell_span} AS mid_x,
                       {WORLD} - (FLOOR(({cy0} + {cy1}) / 2) + 1) * {cell_span} AS mid_y
                FROM split_src
            ), halves AS (
                SELECT cls, minzoom, name, osm_id,
                       ST_CollectionExtract(ST_Intersection(geom,
                           CASE WHEN split_x
                                THEN ST_MakeEnvelope(min_x, min_y, mid_x, max_y)
                                ELSE ST_MakeEnvelope(min_x, mid_y, max_x, max_y)
                           END), 3) AS geom
                FROM m
                UNION ALL
                SELECT cls, minzoom, name, osm_id,
                       ST_CollectionExtract(ST_Intersection(geom,
                           CASE WHEN split_x
                                THEN ST_MakeEnvelope(mid_x, min_y, max_x, max_y)
                                ELSE ST_MakeEnvelope(min_x, min_y, max_x, mid_y)
                           END), 3) AS geom
                FROM m
            )
            SELECT cls, minzoom, name, osm_id, geom,
                   ST_XMin(geom) AS min_x, ST_YMin(geom) AS min_y,
                   ST_XMax(geom) AS max_x, ST_YMax(geom) AS max_y
            FROM halves WHERE NOT ST_IsEmpty(geom);
            DROP TABLE split_src;
            CHECKPOINT;
            "#,
            -WORLD
        ))?;
    }
    con.execute_batch("RESET threads")?;
    // One transaction: between the delete and the insert there is no layer
    // at all, and that is not a state to leave behind if this dies.
    con.execute_batch(&format!(
        r#"
        BEGIN TRANSACTION;
        DELETE FROM features WHERE layer = '{layer}';
        INSERT INTO features (layer, cls, minzoom, name, osm_id, geom,
                              min_x, min_y, max_x, max_y, cell)
        SELECT '{layer}', cls, minzoom, name, osm_id, geom,
               min_x, min_y, max_x, max_y, {cell} AS cell
        FROM layer_sub ORDER BY cell;
        CREATE TABLE {layer}_subdivided (cell_zoom UTINYINT);
        INSERT INTO {layer}_subdivided VALUES ({cell_zoom});
        COMMIT;
        "#,
        cell = tuning::cell_sql_at_maxzoom(),
    ))?;
    con.execute_batch("DROP TABLE layer_sub; CHECKPOINT")?;
    let (n, verts): (i64, i64) = con.query_row(
        &format!(
            "SELECT count(*), max(ST_NPoints(geom)) FROM features WHERE layer = '{layer}'"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    progress::timed(
        format!(
            "{layer} subdivided into {} pieces, largest now {} vertices",
            progress::commas(n as u64),
            progress::commas(verts as u64)
        ),
        t0,
    );
    progress::tick(weight);
    Ok(())
}

/// The coastline, from the one place it exists as polygons rather than as
/// unclosed ways. See `tuning::LAND_URL` for why it is a separate download and
/// why it is the land and not the sea.
///
/// Loaded last because the clip box is the extracts' own extent, which is only
/// known once they are all in. Clipping matters more than it looks: the dataset
/// is global, and a chunk of Newfoundland kept because its bounding box grazes
/// the map's is a chunk that gets simplified, exploded and encoded at every
/// zoom. Read straight out of the zip, already in EPSG:3857, so this is the one
/// source that needs neither unpacking nor reprojecting.
fn land(cfg: &Config, con: &Connection, weight: f64) -> Result<(), Error> {
    let t0 = Instant::now();
    progress::at("clipping the coastline to the extent");
    let (min_x, min_y, max_x, max_y): (f64, f64, f64, f64) = con.query_row(
        "SELECT min(min_x), min(min_y), max(max_x), max(max_y) FROM features",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let minzoom = tuning::minzoom();
    con.execute_batch(&format!(
        r#"
        INSERT INTO features (layer, cls, minzoom, name, osm_id, geom,
                              min_x, min_y, max_x, max_y, cell)
        WITH src AS (
            SELECT geom FROM ST_Read('{shp}')
            WHERE ST_XMax(geom) >= {min_x} AND ST_XMin(geom) <= {max_x}
              AND ST_YMax(geom) >= {min_y} AND ST_YMin(geom) <= {max_y}
        ), clipped AS (
            -- ST_Intersection can hand back a collection where a chunk only
            -- touches the box along an edge; keep the polygonal part, and the
            -- ST_IsEmpty filter drops what is left of the ones that had none.
            SELECT ST_CollectionExtract(
                       ST_Intersection(geom,
                           ST_MakeEnvelope({min_x}, {min_y}, {max_x}, {max_y})), 3) AS geom
            FROM src
        ), bounded AS (
            SELECT geom,
                   ST_XMin(geom) AS min_x, ST_YMin(geom) AS min_y,
                   ST_XMax(geom) AS max_x, ST_YMax(geom) AS max_y
            FROM clipped WHERE NOT ST_IsEmpty(geom)
        )
        SELECT 'land', 'land', {minzoom}, NULL, NULL, geom,
               min_x, min_y, max_x, max_y, {cell} AS cell
        FROM bounded
        ORDER BY cell
        "#,
        shp = cfg.land_shp(),
        cell = tuning::cell_sql_at_maxzoom(),
    ))?;
    let n: i64 = con.query_row(
        "SELECT count(*) FROM features WHERE layer = 'land'",
        [],
        |r| r.get(0),
    )?;
    progress::timed(
        format!("{} land polygons clipped in", progress::commas(n as u64)),
        t0,
    );
    progress::tick(weight);
    Ok(())
}

const TO_3857: &str =
    "ST_Transform(ST_GeomFromWKB(wkb), 'EPSG:4326', 'EPSG:3857', always_xy := true)";

/// Lines: roads, plus rivers/canals which are tagged on ways not areas.
fn classify_lines(con: &Connection) -> Result<(), Error> {
    let maxzoom = tuning::maxzoom();
    con.execute_batch(&format!(
        r#"
        INSERT INTO features
        WITH src AS (
            SELECT osm_id, name, highway, waterway, {to_3857} AS geom
            FROM raw WHERE kind = 'line'
        ), classified AS (
            SELECT osm_id, name, geom,
                   CASE WHEN highway IS NOT NULL THEN 'roads' ELSE 'water' END AS layer,
                   CASE WHEN highway IS NOT NULL THEN {road_class}
                        WHEN waterway IN ('river', 'canal') THEN 'river'
                        ELSE 'stream' END AS cls
            FROM src
        ), bounded AS (
            SELECT layer, cls,
                   CASE WHEN layer = 'roads' THEN {road_minzoom}
                        WHEN cls = 'river' THEN {river_minzoom}
                        ELSE {maxzoom} END AS minzoom,
                   name, osm_id, geom,
                   ST_XMin(geom) AS min_x, ST_YMin(geom) AS min_y,
                   ST_XMax(geom) AS max_x, ST_YMax(geom) AS max_y
            FROM classified
            WHERE NOT ST_IsEmpty(geom)
        )
        SELECT *, {cell} AS cell FROM bounded
        WHERE minzoom <= {maxzoom}
        ORDER BY cell
        "#,
        cell = tuning::cell_sql_at_maxzoom(),
        to_3857 = TO_3857,
        road_class = tuning::road_class_sql("highway"),
        road_minzoom = tuning::road_minzoom_sql("cls"),
        river_minzoom = 8u8.clamp(tuning::minzoom(), tuning::maxzoom()),
    ))?;
    Ok(())
}

/// Areas: water bodies, landuse, buildings. The extractor already stitched
/// multipolygon relations, so holes and multi-part shapes are intact.
fn classify_areas(con: &Connection) -> Result<(), Error> {
    let maxzoom = tuning::maxzoom();
    con.execute_batch(&format!(
        r#"
        INSERT INTO features
        WITH src AS (
            SELECT osm_id, name, building, landuse, "natural", water, waterway,
                   leisure, {to_3857} AS geom
            FROM raw WHERE kind = 'area'
        ), valid AS (
            -- OSM rings self-intersect often enough that this is not an edge
            -- case: Luxembourg has two woods that GEOS refuses to clip, and it
            -- refuses by throwing, which fails the whole bake. Repairing here
            -- costs one pass at load; repairing in the bake would cost one per
            -- zoom per layer. MakeValid can demote a polygon to a collection of
            -- lines and points, so keep only the polygonal part -- and if that
            -- leaves nothing, the ST_IsEmpty filter below drops the feature.
            SELECT * REPLACE (
                CASE WHEN ST_IsValid(geom) THEN geom
                     ELSE ST_CollectionExtract(ST_MakeValid(geom), 3) END AS geom
            ) FROM src
        ), classified AS (
            SELECT osm_id, name, geom, {poly_class} AS cls FROM valid
        ), bounded AS (
            SELECT {poly_layer} AS layer, cls, {area_minzoom} AS minzoom,
                   name, osm_id, geom,
                   ST_XMin(geom) AS min_x, ST_YMin(geom) AS min_y,
                   ST_XMax(geom) AS max_x, ST_YMax(geom) AS max_y
            FROM classified
            WHERE cls IS NOT NULL AND NOT ST_IsEmpty(geom)
        )
        SELECT *, {cell} AS cell FROM bounded
        WHERE minzoom <= {maxzoom}
        ORDER BY cell
        "#,
        cell = tuning::cell_sql_at_maxzoom(),
        to_3857 = TO_3857,
        poly_class = tuning::POLY_CLASS_SQL,
        poly_layer = tuning::POLY_LAYER_SQL,
        area_minzoom = tuning::area_minzoom_sql(),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monster polygon spanning many cells must come out as many small
    /// pieces that still carry their parent's cls and minzoom; a small one
    /// must pass through untouched.
    #[test]
    fn subdivide_water_bounds_the_pieces() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch("INSTALL spatial; LOAD spatial;").unwrap();
        con.execute_batch(
            "CREATE TABLE features (
                layer VARCHAR, cls VARCHAR, minzoom UTINYINT, name VARCHAR,
                osm_id BIGINT, geom GEOMETRY,
                min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE,
                cell BIGINT)",
        )
        .unwrap();
        // ~50 km on each side: dozens of maxzoom-3 cells.
        con.execute_batch(
            "INSERT INTO features
             SELECT 'water', 'water', 10, 'big lake', 42, geom,
                    ST_XMin(geom), ST_YMin(geom), ST_XMax(geom), ST_YMax(geom), NULL
             FROM (SELECT ST_GeomFromText(
                 'POLYGON((0 0, 50000 0, 50000 50000, 0 50000, 0 0))') AS geom);
             INSERT INTO features
             SELECT 'water', 'water', 14, NULL, 43, geom,
                    ST_XMin(geom), ST_YMin(geom), ST_XMax(geom), ST_YMax(geom), NULL
             FROM (SELECT ST_GeomFromText(
                 'POLYGON((100 100, 200 100, 200 200, 100 200, 100 100))') AS geom);",
        )
        .unwrap();
        subdivide(&con, "water", 0.0).unwrap();

        let (n, area, small): (i64, f64, i64) = con
            .query_row(
                "SELECT count(*), sum(ST_Area(geom)),
                        count(*) FILTER (minzoom = 14 AND osm_id = 43)
                 FROM features WHERE layer = 'water'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(n > 100, "monster should shatter into many pieces, got {n}");
        assert!((area - 2_500_000_000.0 - 10_000.0).abs() < 1.0, "area must be conserved, got {area}");
        assert_eq!(small, 1, "the small polygon passes through whole");
        // Every piece keeps its parent's minzoom.
        let z10: i64 = con
            .query_row(
                "SELECT count(*) FROM features WHERE layer = 'water' AND minzoom = 10",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(z10, n - 1);
        // Bisection ran to completion: no piece straddles a cell boundary.
        let span = tuning::tile_span(
            tuning::maxzoom().saturating_sub(3).max(tuning::minzoom()),
        );
        let w = tuning::WORLD;
        let oversized: i64 = con
            .query_row(
                &format!(
                    "SELECT count(*) FROM features WHERE layer = 'water'
                     AND (FLOOR((max_x + {w}) / {span} - 1e-9) > FLOOR((min_x + {w}) / {span} + 1e-9)
                       OR FLOOR(({w} - min_y) / {span} - 1e-9) > FLOOR(({w} - max_y) / {span} + 1e-9))"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(oversized, 0, "every piece must fit one cell");
        // Idempotent: the marker makes a second call a no-op.
        subdivide(&con, "water", 0.0).unwrap();
        let again: i64 = con
            .query_row("SELECT count(*) FROM features WHERE layer = 'water'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, n);
    }
}
