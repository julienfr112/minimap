//! `features` -> `tiles`. One MVT blob per (z, x, y), built entirely in SQL.

use std::time::Instant;

use duckdb::Connection;

use crate::load;
use crate::progress::{self, Step};
use crate::tuning::{self, BUFFER, EXTENT, LAYERS, MPP0, WORLD};

type Error = Box<dyn std::error::Error>;

/// Half-open tile-column ranges `(lo, hi)` to process one band at a time at
/// zoom `z`, each carrying about as much grid as a whole zoom `budget` would.
///
/// A statement carrying a whole zoom is what breaks at z14: the bake spilled
/// more than 80 GB to temp storage. Bands bound it.
///
/// The slices are uniform across the *world*, but an extract only covers part
/// of it -- Europe reaches from the Azores to Georgia, so at a z12 budget only
/// 5 of 16 bands held anything and one of them carried half the zoom (z14
/// landuse: 4415s of 9075s in band 9). The fix is just more, narrower bands: an
/// empty band costs 0.0s, and the only real price is that a feature straddling
/// a boundary is simplified once per band it touches. Measured across all 270M
/// European features that price is nothing -- at 64 bands the duplication
/// factor is 1.0006 for landuse and water, 1.0000 for buildings.
fn bands(z: u8, budget: u8) -> Vec<(u64, u64)> {
    let slices = 1u64 << (2 * u32::from(z.saturating_sub(budget)));
    let cols = (1u64 << z) / slices; // tile columns per band
    (0..slices).map(|s| (s * cols, (s + 1) * cols)).collect()
}

/// The bands that can actually hold something, each with its index among all of
/// them.
///
/// Bands slice the *world*, and an extract covers a sliver of it, so most of
/// them are empty. That was affordable while the deepest rung was z14 (64 bands,
/// ~14 of them holding Europe) and stops being affordable the moment a rung goes
/// deeper: z17 is 4,096 bands, of which Picardie spans 34. The other 4,062 are
/// empty statements that still cost a round trip -- 16k of them across the
/// layers, for nothing.
///
/// The index among all bands is returned alongside, so a caller that records
/// which band it finished can use a number that does not shift when the extent
/// changes.
fn live_bands(z: u8, budget: u8, min_x: f64, max_x: f64) -> Vec<(usize, u64, u64)> {
    let span = tuning::tile_span(z);
    bands(z, budget)
        .into_iter()
        .enumerate()
        .filter(|(_, (lo, hi))| {
            let (from, to) = (-WORLD + *lo as f64 * span, -WORLD + *hi as f64 * span);
            to > min_x && from <= max_x
        })
        .map(|(i, (lo, hi))| (i, lo, hi))
        .collect()
}

/// One bake band is a z11's worth of grid: 64 bands at z14, of which ~14 hold
/// any of Europe. What bounds this is geometry work per band, not the tile
/// count.
const BAKE_BUDGET: u8 = 11;

/// How long a band has to take before it is worth a line of its own. Below
/// this it held nothing, which is the common case: bands slice the whole world
/// and an extract covers a slice of it.
const CHATTY: std::time::Duration = std::time::Duration::from_millis(500);

/// What one zoom's scan of `features` costs, relative to the deepest zoom's
/// tile work. Every zoom reads the whole table -- in bands, but all of it --
/// whether it emits two tiles or sixteen thousand.
///
/// This term is what makes the early estimate usable. Without it the six
/// cheapest zooms carry 1.6% of the weight while taking 17% of the wall clock,
/// so the first ETA came out eight times too long before correcting itself.
const SCAN_SHARE: f64 = 1.0 / 64.0;

/// Relative cost of one zoom.
///
/// A zoom has ~4x the tiles of the one above it and costs about that much more
/// to clip and encode, plus the scan above. That predicts the shape of a bake
/// well enough to say "twenty minutes left" and be roughly right, which is all
/// an ETA is for. It is the only model here that could be called a guess, which
/// is why the display says `~`.
fn zoom_weight(z: u8) -> f64 {
    let deepest = 4f64.powi(i32::from(tuning::maxzoom() - tuning::minzoom()));
    4f64.powi(i32::from(z.saturating_sub(tuning::minzoom()))) + SCAN_SHARE * deepest
}

/// The whole bake, for one zoom, one layer, one band: an INSERT into `into` of
/// `(z, x, y, layer, mvt)` rows.
fn band_sql(z: u8, layer: &str, lo: u64, hi: u64, into: &str) -> String {
    let tile_span = tuning::tile_span(z);
    let tolerance = MPP0 / (1u32 << z) as f64; // simplify to roughly one pixel
    let hi_m1 = hi - 1;
    let band_min_x = -WORLD + lo as f64 * tile_span;
    let band_max_x = -WORLD + hi as f64 * tile_span;

    // Areas may be simplified into invalid rings; lines cannot. Use the
    // topology-preserving variant for polygons so ST_AsMVTGeom does not choke
    // on self-intersections.
    let simplify = if tuning::is_area(layer) {
        format!("ST_SimplifyPreserveTopology(geom, {tolerance})")
    } else {
        format!("ST_Simplify(geom, {tolerance})")
    };
    // Even the topology-preserving simplifier can emit an invalid ring (one
    // building near Rivne does it at z12, across all of Europe), and GEOS
    // answers by throwing out of ST_AsMVTGeom, failing the whole bake. Same
    // repair as the load, applied to what simplification changed; lines cannot
    // go invalid, so they skip it.
    let repair = if tuning::is_area(layer) {
        "CASE WHEN ST_IsValid(geom) THEN geom
              ELSE ST_CollectionExtract(ST_MakeValid(geom), 3) END"
    } else {
        "geom"
    };
    let bounds = format!(
        "{{'min_x': {} + x * {tile_span},
           'min_y': {WORLD} - (y + 1) * {tile_span},
           'max_x': {} + (x + 1) * {tile_span},
           'max_y': {WORLD} - y * {tile_span}}}::BOX_2D",
        -WORLD, -WORLD
    );
    // Only the label layer carries names through; see tuning::NAMED_LAYER.
    let (named, name_prop) = if layer == tuning::NAMED_LAYER {
        (", name", ", name: name")
    } else {
        ("", "")
    };
    format!(
        r#"
        INSERT INTO {into}
        WITH candidates AS (
            -- Which tiles can this feature touch? Pure bbox arithmetic, so no
            -- spatial join and no index is needed. The clamps pin boundary
            -- features to this band's columns, so a feature overlapping two
            -- bands never emits the same tile twice.
            SELECT cls{named}, {simplify} AS geom,
                   GREATEST(CAST(FLOOR((min_x + {WORLD}) / {tile_span}) AS INTEGER), {lo}) AS x0,
                   LEAST(CAST(FLOOR((max_x + {WORLD}) / {tile_span}) AS INTEGER), {hi_m1}) AS x1,
                   CAST(FLOOR(({WORLD} - max_y) / {tile_span}) AS INTEGER) AS y0,
                   CAST(FLOOR(({WORLD} - min_y) / {tile_span}) AS INTEGER) AS y1
            FROM features
            WHERE layer = '{layer}' AND minzoom <= {z}
              AND max_x >= {band_min_x} AND min_x < {band_max_x}
        ), repaired AS (
            SELECT cls{named}, {repair} AS geom, x0, x1, y0, y1 FROM candidates
        ), exploded AS (
            SELECT cls{named}, geom, x, y
            FROM repaired,
                 UNNEST(range(x0, x1 + 1)) AS _x(x),
                 UNNEST(range(y0, y1 + 1)) AS _y(y)
        ), clipped AS (
            SELECT x, y, cls{named},
                   ST_AsMVTGeom(geom, {bounds}, {EXTENT}, {BUFFER}, true) AS g
            FROM exploded
        )
        -- `cls` is all the styling needs, and for every layer but the label one
        -- it is the only attribute a tile carries: names on every feature
        -- inflate a tile by roughly 40% to write text nothing draws.
        SELECT {z}, x, y, '{layer}',
               ST_AsMVT({{geom: g, cls: cls{name_prop}}}, '{layer}', {EXTENT}, 'geom')
        FROM clipped
        WHERE g IS NOT NULL AND NOT ST_IsEmpty(g)
        GROUP BY x, y
        "#
    )
}

pub fn run(con: &Connection) -> Result<(), Error> {
    let n: i64 = con
        .query_row("SELECT count(*) FROM features", [], |r| r.get(0))
        .map_err(|_| "no `features` table -- run `make load` first")?;
    if n == 0 {
        return Err("features table is empty -- run `make load` first".into());
    }
    // A deep rung prices a feature at (vertices x tiles covered), which only
    // subdivision keeps bounded -- see load::subdivide. Marker-gated, so this
    // is a no-op when the load already did it; it runs here for databases
    // loaded before their layer got that treatment (water, historically).
    load::subdivide(con, "land", 0.0)?;
    load::subdivide(con, "water", 0.0)?;
    // The data's own horizontal extent, so the band loops can skip the parts of
    // the world nothing was loaded for. One query, and at a deep rung it is the
    // difference between 34 statements and 4,096.
    let (data_min_x, data_max_x): (f64, f64) = con.query_row(
        "SELECT min(min_x), max(max_x) FROM features",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let step = Step::start(
        "bake",
        format!(
            "{} features -> MVT rungs z{}",
            progress::commas(n as u64),
            tuning::zooms_csv()
        ),
    );
    progress::line(format!(
        "background layers ({}) capped at z{}",
        LAYERS
            .iter()
            .filter(|l| tuning::is_background(l))
            .copied()
            .collect::<Vec<_>>()
            .join(", "),
        tuning::BACKGROUND_MAXZOOM
    ));

    // Every per-layer INSERT below is one transaction, so a crash mid-bake
    // (historically ENOSPC in temp storage) loses only the statement in
    // flight. `bake_done` records the zooms whose layers all committed; when
    // it exists, pick up after the last finished zoom instead of starting
    // over -- z6..z13 for Europe is two hours of work.
    let resume: Option<u8> = con
        .query_row("SELECT max(z) FROM bake_done", [], |r| {
            r.get::<_, Option<u8>>(0)
        })
        .ok()
        .flatten();
    // Resume at the first rung after the last one that fully committed.
    let start = if let Some(z) = resume.filter(|z| *z >= tuning::minzoom()) {
        con.execute_batch(&format!("DELETE FROM tile_layers WHERE z > {z}"))?;
        progress::line(format!("resuming after z{z}"));
        z + 1
    } else {
        // Starting over invalidates anything a previous run left behind: it
        // describes features this one is about to replace.
        con.execute_batch(
            "DROP TABLE IF EXISTS tile_layers;
             DROP TABLE IF EXISTS tiles;
             DROP TABLE IF EXISTS merge_done;
             CREATE TABLE tile_layers
                 (z UTINYINT, x UINTEGER, y UINTEGER, layer VARCHAR, data BLOB);
             CREATE OR REPLACE TABLE bake_done (z UTINYINT)",
        )?;
        tuning::minzoom()
    };

    // Weighted by rung, so the bar reflects work rather than levels done.
    let rungs: Vec<u8> = tuning::ZOOMS.into_iter().filter(|z| *z >= start).collect();
    let tiling: f64 = rungs
        .iter()
        .map(|z| zoom_weight(*z))
        .sum();
    progress::begin(tiling);

    for z in rungs {
        let t0 = Instant::now();

        // Features are filtered on their bbox before simplification, so a band
        // only pays for what overlaps it.
        let all = live_bands(z, BAKE_BUDGET, data_min_x, data_max_x);
        let slices = all.len().max(1);
        let baked: Vec<&str> = LAYERS.iter().copied().filter(|l| tuning::bakes(l, z)).collect();
        let unit =
            zoom_weight(z) / (baked.len().max(1) * slices) as f64;

        for layer in baked {
            for (n, &(_, lo, hi)) in all.iter().enumerate() {
                let t1 = Instant::now();
                progress::at(format!("z{z} {layer} band {}/{slices}", n + 1));
                con.execute_batch(&band_sql(z, layer, lo, hi, "tile_layers"))?;
                progress::tick(unit);
                // A sliced zoom is hours of work with nothing on the terminal;
                // one line per band shows where it is. Only for the bands that
                // actually cost something, though: at z14 there are 64 bands
                // per layer and an extract covers a handful of them, so
                // reporting every one buried Picardie's whole bake in 588 lines
                // of `0.0s` — and buried Europe's real ones with it.
                if slices > 1 && t1.elapsed() >= CHATTY {
                    progress::timed(format!("z{z} {layer} band {}/{slices}", n + 1), t1);
                }
            }
        }

        con.execute_batch(&format!("INSERT INTO bake_done VALUES ({z})"))?;
        let made: i64 = con.query_row(
            "SELECT count(DISTINCT (x, y)) FROM tile_layers WHERE z = ?",
            [z],
            |r| r.get(0),
        )?;
        progress::timed(
            format!("z{z}  {} tiles", progress::commas(made as u64)),
            t0,
        );
    }

    // `tile_layers` is the deliverable now, not scaffolding: export writes one
    // archive per layer straight out of it. Deleting the merge that used to turn
    // it into a single `tiles` table removed the most fragile statement in the
    // pipeline -- the one whose obvious spelling held all 22 GB of Europe at once
    // and died at 18.4 GiB -- and halved the bake's peak disk, since the two
    // tables no longer have to exist at the same time.
    progress::at("indexing tiles");
    con.execute_batch(
        "DROP TABLE IF EXISTS tiles;
         DROP TABLE IF EXISTS merge_done;
         DROP TABLE bake_done;
         CREATE INDEX IF NOT EXISTS tile_layers_lzxy ON tile_layers (layer, z, x, y);
         CHECKPOINT",
    )?;
    bounds(con)?;
    progress::end();

    let mut stmt = con.prepare(
        "SELECT layer, count(*), sum(octet_length(data))
         FROM tile_layers GROUP BY layer ORDER BY 3 DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    println!();
    for row in rows {
        let (layer, n, bytes) = row?;
        progress::line(format!(
            "{layer:<12} {:>10} tiles  {:>10}",
            progress::commas(n as u64),
            progress::bytes(bytes as u64)
        ));
    }
    step.done();
    Ok(())
}

/// Bounds for the viewer, back in WGS84. ST_Extent returns a BOX_2D whose
/// fields are not struct-accessible, so read the corners off the geometry.
fn bounds(con: &Connection) -> Result<(), Error> {
    con.execute_batch(&format!(
        r#"
        DROP TABLE IF EXISTS meta;
        CREATE TABLE meta AS
        WITH b AS (
            SELECT ST_Transform(ST_Extent_Agg(geom)::GEOMETRY,
                                'EPSG:3857', 'EPSG:4326', always_xy := true) AS g
            FROM features
        )
        SELECT ST_XMin(g) AS west, ST_YMin(g) AS south,
               ST_XMax(g) AS east, ST_YMax(g) AS north
        FROM b
        "#
    ))?;
    Ok(())
}
