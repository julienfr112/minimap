//! One-off migration: add the `land` layer to an archive that was baked without
//! it, without rebaking the rest.
//!
//!   cargo run --release --example add-land && cargo run --release --bin minimap export
//!
//! New builds need none of this. `land` is in `config::LAYERS` and `load::run`
//! reads the coastline, so `minimap all` produces it from scratch. This exists
//! for the one archive that already cost eight hours of bake, whose four layers
//! are all still correct: nothing about a building changes because the sea
//! arrived. A Tile protobuf is `repeated Layer layers = 3` and land draws first,
//! so the new tile is `land || existing` -- a concatenation, not a re-encode.
//!
//! Three phases, each resumable, because the whole point is not to lose work:
//!
//!   1. read the coastline into `features` (skipped if it is already there)
//!   2. bake the land layer alone into `land_layers`, band by band
//!   3. splice: `tiles` <- land || tiles, band by band
//!
//! Phase 3 is a full outer join, not a left join. Land reaches tiles that have
//! no OSM features at all -- an empty Norwegian fell, an uninhabited Aegean
//! island -- and those are exactly the tiles that would otherwise render as sea.

use std::time::Instant;

use duckdb::Connection;
use minimap::bake;
use minimap::config::{self, MAXZOOM, MINZOOM};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let con = config::connect(false)?;

    // Running this twice would be worse than running it never: the splice drops
    // its scaffolding on success, so a second run would recreate it empty, bake
    // all nine zooms over again, and concatenate a second copy of land into
    // tiles that already have one.
    if applied(&con)? {
        config::log("land is already merged into tiles -- nothing to do");
        return Ok(());
    }

    features(&con)?;
    // Cheap and idempotent, and it is what makes z14 finish this side of
    // tomorrow -- see load::subdivide_land.
    minimap::load::subdivide_land(&con)?;
    bake_land(&con)?;
    splice(&con)?;

    let (n, bytes): (i64, i64) = con.query_row(
        "SELECT count(*), sum(octet_length(data)) FROM tiles",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    config::log(format!(
        "{} tiles, {:.1} GB -- now re-run `minimap export`",
        config::commas(n as u64),
        bytes as f64 / 1e9
    ));
    Ok(())
}

/// Has a previous run already merged land into `tiles`?
///
/// A single marker, written in the same transaction as the table swap, rather
/// than anything inferred from the scaffolding: "subdivided but no scaffolding"
/// looks identical whether the splice finished or the process died in the
/// seventy-five seconds between subdividing and creating the first band table.
fn applied(con: &Connection) -> Result<bool, Box<dyn std::error::Error>> {
    let marker: i64 = con.query_row(
        "SELECT count(*) FROM duckdb_tables() WHERE table_name = 'land_merged'",
        [],
        |r| r.get(0),
    )?;
    Ok(marker > 0)
}

/// Phase 1. Same clip box the bake's `meta` already published, so the sea stops
/// where the map does.
fn features(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let have: i64 = con.query_row(
        "SELECT count(*) FROM features WHERE layer = 'land'",
        [],
        |r| r.get(0),
    )?;
    if have > 0 {
        config::log(format!(
            "{} land polygons already loaded",
            config::commas(have as u64)
        ));
        return Ok(());
    }
    let t0 = Instant::now();
    let (min_x, min_y, max_x, max_y): (f64, f64, f64, f64) = con.query_row(
        "SELECT min(min_x), min(min_y), max(max_x), max(max_y) FROM features",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    con.execute_batch(&format!(
        r#"
        INSERT INTO features (layer, cls, minzoom, name, osm_id, geom,
                              min_x, min_y, max_x, max_y, cell)
        WITH src AS (
            SELECT geom FROM ST_Read('{shp}')
            WHERE ST_XMax(geom) >= {min_x} AND ST_XMin(geom) <= {max_x}
              AND ST_YMax(geom) >= {min_y} AND ST_YMin(geom) <= {max_y}
        ), clipped AS (
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
        SELECT 'land', 'land', {MINZOOM}, NULL, NULL, geom,
               min_x, min_y, max_x, max_y, {cell} AS cell
        FROM bounded
        ORDER BY cell
        "#,
        shp = config::land_shp(),
        cell = config::cell_sql(2.0 * config::WORLD / (1u32 << MAXZOOM) as f64),
    ))?;
    let n: i64 = con.query_row(
        "SELECT count(*) FROM features WHERE layer = 'land'",
        [],
        |r| r.get(0),
    )?;
    config::timed(
        format!("{} land polygons clipped in", config::commas(n as u64)),
        t0,
    );
    Ok(())
}

/// Phase 2. `bake::band_sql` is the pipeline's own statement, so these tiles are
/// encoded exactly as a full rebake would encode them.
fn bake_land(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    con.execute_batch(
        "CREATE TABLE IF NOT EXISTS land_layers
             (z UTINYINT, x UINTEGER, y UINTEGER, layer VARCHAR, data BLOB);
         CREATE TABLE IF NOT EXISTS land_done (z UTINYINT, band UBIGINT)",
    )?;
    for z in MINZOOM..=MAXZOOM {
        let t0 = Instant::now();
        let all = bake::bake_bands(z);
        let n = all.len();
        for (s, &(lo, hi)) in all.iter().enumerate() {
            let done: i64 = con.query_row(
                &format!("SELECT count(*) FROM land_done WHERE z = {z} AND band = {s}"),
                [],
                |r| r.get(0),
            )?;
            if done > 0 {
                continue;
            }
            let t1 = Instant::now();
            con.execute_batch(&format!(
                "BEGIN TRANSACTION;\n{}\n;INSERT INTO land_done VALUES ({z}, {s});\nCOMMIT;",
                bake::band_sql(z, "land", lo, hi, "land_layers")
            ))?;
            if n > 1 {
                config::timed(format!("z{z} land band {}/{n}", s + 1), t1);
            }
        }
        let made: i64 =
            con.query_row("SELECT count(*) FROM land_layers WHERE z = ?", [z], |r| {
                r.get(0)
            })?;
        config::timed(
            format!("z{z}: {} land tiles", config::commas(made as u64)),
            t0,
        );
    }
    Ok(())
}

/// Phase 3. Rebuilt rather than updated in place: DuckDB rewrites the row either
/// way, and a new table can be checked against the old one before the swap.
fn splice(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    con.execute_batch(
        "CREATE TABLE IF NOT EXISTS tiles_land
             (z UTINYINT, x UINTEGER, y UINTEGER, data BLOB);
         CREATE TABLE IF NOT EXISTS splice_done (z UTINYINT, band UBIGINT)",
    )?;
    let t0 = Instant::now();
    for z in MINZOOM..=MAXZOOM {
        let all = bake::bake_bands(z);
        let n = all.len();
        for (s, &(lo, hi)) in all.iter().enumerate() {
            let done: i64 = con.query_row(
                &format!("SELECT count(*) FROM splice_done WHERE z = {z} AND band = {s}"),
                [],
                |r| r.get(0),
            )?;
            if done > 0 {
                continue;
            }
            let t1 = Instant::now();
            con.execute_batch(&format!(
                r#"
                BEGIN TRANSACTION;
                INSERT INTO tiles_land
                SELECT z, x, y,
                       COALESCE(l.data, ''::BLOB) || COALESCE(t.data, ''::BLOB)
                FROM (SELECT z, x, y, data FROM land_layers
                      WHERE z = {z} AND x >= {lo} AND x < {hi}) l
                FULL OUTER JOIN
                     (SELECT z, x, y, data FROM tiles
                      WHERE z = {z} AND x >= {lo} AND x < {hi}) t
                USING (z, x, y);
                INSERT INTO splice_done VALUES ({z}, {s});
                COMMIT;
                "#
            ))?;
            if n > 1 {
                config::timed(format!("z{z} splice band {}/{n}", s + 1), t1);
            }
        }
    }
    // Only now is the old table redundant. The index goes with it; `export`
    // reads the table whole and does not use it, but `info` and any ad-hoc
    // lookup do.
    // One transaction: DuckDB's DDL is transactional, and these statements
    // individually are not a state anyone wants to wake up to. A kill between
    // the drop and the rename would leave the database with no `tiles` at all,
    // having just spent a night computing one.
    con.execute_batch(
        "BEGIN TRANSACTION;
         DROP TABLE tiles;
         ALTER TABLE tiles_land RENAME TO tiles;
         DROP TABLE land_layers;
         DROP TABLE land_done;
         DROP TABLE splice_done;
         CREATE INDEX tiles_zxy ON tiles (z, x, y);
         CREATE TABLE land_merged (tiles BIGINT);
         INSERT INTO land_merged SELECT count(*) FROM tiles;
         COMMIT;",
    )?;
    con.execute_batch("CHECKPOINT")?;
    config::timed("land spliced into tiles", t0);
    Ok(())
}
