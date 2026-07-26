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

use crate::config::{self, MAXZOOM, WORLD};
use crate::extract;
use crate::rows::RawTable;

pub fn run(con: &Connection, regions: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let missing: Vec<&String> =
        regions.iter().filter(|r| !config::pbf_path(r).exists()).collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing extracts: {missing:?} -- run `minimap download` or ./fetch-europe.sh first"
        )
        .into());
    }

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
    con.execute_batch(config::BIT_SPREAD_MACRO)?;
    for region in regions {
        let t0 = Instant::now();
        con.execute_batch(&format!(
            "DROP TABLE IF EXISTS raw; CREATE TABLE raw ({})",
            config::RAW_DDL
        ))?;
        let mut sink = RawTable::new(con.appender("raw")?);
        extract::run(&config::pbf_path(region), &mut sink)?;
        let n = sink.finish()?;
        config::timed(format!("{region}: {} objects staged", config::commas(n)), t0);

        let t0 = Instant::now();
        classify_lines(con)?;
        classify_areas(con)?;
        con.execute_batch("DROP TABLE raw; CHECKPOINT")?;
        let total: i64 = con.query_row("SELECT count(*) FROM features", [], |r| r.get(0))?;
        config::timed(
            format!("{region}: classified, {} features so far", config::commas(total as u64)),
            t0,
        );
    }

    // Adjacent extracts overlap along their shared border, so the same way can
    // arrive twice. Deleted in place -- see the note at the top of this file for
    // why this is not a rewrite.
    let t0 = Instant::now();
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
    config::timed(
        format!(
            "{} -> {} features, duplicates removed",
            config::commas(before as u64),
            config::commas(after as u64)
        ),
        t0,
    );

    let mut stmt = con.prepare(
        "SELECT layer, count(*) n, min(minzoom), max(minzoom)
         FROM features GROUP BY layer ORDER BY n DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
    })?;
    for row in rows {
        let (layer, n, lo, hi) = row?;
        config::log(format!("  {layer:<10} {:>9}  minzoom {lo}..{hi}", config::commas(n as u64)));
    }
    Ok(())
}

/// Morton code of a feature's bbox centre on the MAXZOOM tile grid.
fn cell() -> String {
    config::cell_sql(2.0 * WORLD / (1u32 << MAXZOOM) as f64)
}

const TO_3857: &str =
    "ST_Transform(ST_GeomFromWKB(wkb), 'EPSG:4326', 'EPSG:3857', always_xy := true)";

/// Lines: roads, plus rivers/canals which are tagged on ways not areas.
fn classify_lines(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
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
                        WHEN cls = 'river' THEN 8
                        ELSE {MAXZOOM} END AS minzoom,
                   name, osm_id, geom,
                   ST_XMin(geom) AS min_x, ST_YMin(geom) AS min_y,
                   ST_XMax(geom) AS max_x, ST_YMax(geom) AS max_y
            FROM classified
            WHERE NOT ST_IsEmpty(geom)
        )
        SELECT *, {cell} AS cell FROM bounded
        WHERE minzoom <= {MAXZOOM}
        ORDER BY cell
        "#,
        cell = cell(),
        to_3857 = TO_3857,
        road_class = config::road_class_sql("highway"),
        road_minzoom = config::road_minzoom_sql("cls"),
    ))?;
    Ok(())
}

/// Areas: water bodies, landuse, buildings. The extractor already stitched
/// multipolygon relations, so holes and multi-part shapes are intact.
fn classify_areas(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
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
        WHERE minzoom <= {MAXZOOM}
        ORDER BY cell
        "#,
        cell = cell(),
        to_3857 = TO_3857,
        poly_class = config::POLY_CLASS_SQL,
        poly_layer = config::POLY_LAYER_SQL,
        area_minzoom = config::area_minzoom_sql(),
    ))?;
    Ok(())
}
