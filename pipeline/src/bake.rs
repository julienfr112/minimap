//! `features` -> `tiles`. One MVT blob per (z, x, y), built entirely in SQL.

use std::time::Instant;

use duckdb::Connection;

use crate::config::{self, BUFFER, EXTENT, LAYERS, MAXZOOM, MINZOOM, MPP0, WORLD};

pub fn run(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let n: i64 = con.query_row("SELECT count(*) FROM features", [], |r| r.get(0))?;
    if n == 0 {
        return Err("features table is empty -- run `minimap load` first".into());
    }

    con.execute_batch(
        "DROP TABLE IF EXISTS tile_layers;
         CREATE TABLE tile_layers
             (z UTINYINT, x UINTEGER, y UINTEGER, layer VARCHAR, data BLOB)",
    )?;

    for z in MINZOOM..=MAXZOOM {
        let tile_span = 2.0 * WORLD / (1u32 << z) as f64; // projected size of one tile
        let tolerance = MPP0 / (1u32 << z) as f64; // simplify to roughly one pixel
        let t0 = Instant::now();

        for layer in LAYERS {
            // Areas may be simplified into invalid rings; lines cannot. Use the
            // topology-preserving variant for polygons so ST_AsMVTGeom does not
            // choke on self-intersections.
            let simplify = if matches!(layer, "landuse" | "water" | "buildings") {
                format!("ST_SimplifyPreserveTopology(geom, {tolerance})")
            } else {
                format!("ST_Simplify(geom, {tolerance})")
            };
            let bounds = format!(
                "{{'min_x': {} + x * {tile_span},
                   'min_y': {WORLD} - (y + 1) * {tile_span},
                   'max_x': {} + (x + 1) * {tile_span},
                   'max_y': {WORLD} - y * {tile_span}}}::BOX_2D",
                -WORLD, -WORLD
            );
            con.execute_batch(&format!(
                r#"
                INSERT INTO tile_layers
                WITH candidates AS (
                    -- Which tiles can this feature touch? Pure bbox arithmetic,
                    -- so no spatial join and no index is needed.
                    SELECT cls, {simplify} AS geom,
                           CAST(FLOOR((min_x + {WORLD}) / {tile_span}) AS INTEGER) AS x0,
                           CAST(FLOOR((max_x + {WORLD}) / {tile_span}) AS INTEGER) AS x1,
                           CAST(FLOOR(({WORLD} - max_y) / {tile_span}) AS INTEGER) AS y0,
                           CAST(FLOOR(({WORLD} - min_y) / {tile_span}) AS INTEGER) AS y1
                    FROM features
                    WHERE layer = '{layer}' AND minzoom <= {z}
                ), exploded AS (
                    SELECT cls, geom, x, y
                    FROM candidates,
                         UNNEST(range(x0, x1 + 1)) AS _x(x),
                         UNNEST(range(y0, y1 + 1)) AS _y(y)
                ), clipped AS (
                    SELECT x, y, cls,
                           ST_AsMVTGeom(geom, {bounds}, {EXTENT}, {BUFFER}, true) AS g
                    FROM exploded
                )
                -- `cls` is the only attribute a tile carries: it is all the
                -- styling needs. Names stay in `features` (we draw no labels);
                -- adding them here would inflate every tile by roughly 40%.
                SELECT {z}, x, y, '{layer}',
                       ST_AsMVT({{geom: g, cls: cls}}, '{layer}', {EXTENT}, 'geom')
                FROM clipped
                WHERE g IS NOT NULL AND NOT ST_IsEmpty(g)
                GROUP BY x, y
                "#
            ))?;
        }

        let made: i64 = con.query_row(
            "SELECT count(DISTINCT (x, y)) FROM tile_layers WHERE z = ?",
            [z],
            |r| r.get(0),
        )?;
        config::timed(format!("z{z}: {} tiles", config::commas(made as u64)), t0);
    }

    // A Tile protobuf is just `repeated Layer layers = 3`, so concatenating the
    // per-layer blobs in draw order yields one valid multi-layer tile. That is
    // what lets the backend stay a pure blob lookup.
    let t0 = Instant::now();
    let order: String =
        LAYERS.iter().enumerate().map(|(i, l)| format!("WHEN '{l}' THEN {i}")).collect::<Vec<_>>().join(" ");
    con.execute_batch(&format!(
        r#"
        CREATE OR REPLACE TABLE tiles AS
        SELECT z, x, y,
               list_reduce(list(data ORDER BY CASE layer {order} END),
                           (a, b) -> a || b) AS data
        FROM tile_layers
        GROUP BY z, x, y;
        DROP TABLE tile_layers;
        CREATE INDEX tiles_zxy ON tiles (z, x, y);
        "#
    ))?;
    config::timed("layers merged into tiles", t0);

    // Bounds for the viewer, back in WGS84. ST_Extent returns a BOX_2D whose
    // fields are not struct-accessible, so read the corners off the geometry.
    con.execute_batch(&format!(
        r#"
        DROP TABLE IF EXISTS meta;
        CREATE TABLE meta AS
        WITH b AS (
            SELECT ST_Transform(ST_Extent_Agg(geom)::GEOMETRY,
                                'EPSG:3857', 'EPSG:4326', always_xy := true) AS g
            FROM features
        )
        SELECT {MINZOOM} AS minzoom, {MAXZOOM} AS maxzoom,
               ST_XMin(g) AS west, ST_YMin(g) AS south,
               ST_XMax(g) AS east, ST_YMax(g) AS north
        FROM b
        "#
    ))?;

    let (count, total, largest): (i64, i64, i64) = con.query_row(
        "SELECT count(*), sum(octet_length(data)), max(octet_length(data)) FROM tiles",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    config::log(format!(
        "{} tiles, {:.1} MB total, largest {:.0} kB",
        config::commas(count as u64),
        total as f64 / 1e6,
        largest as f64 / 1e3
    ));
    Ok(())
}
