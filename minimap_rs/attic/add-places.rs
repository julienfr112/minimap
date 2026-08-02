//! One-off migration: add the `places` label layer to an archive already baked
//! without it.
//!
//!   cargo run --release --example add-places && cargo run --release --bin minimap export
//!
//! Same shape as add-land.rs, and the same reasoning: the existing layers are
//! all still correct, so bake only the new one and concatenate. Labels draw
//! *last*, so here the new tile is `existing || places` -- the append rather
//! than the prepend.
//!
//! Cheap by comparison with land: a continent has tens of thousands of cities,
//! not half a million coastline chunks. The cost is the scan of the extracts,
//! which has to decompress every PBF to find nodes that were never staged.
//!
//! New builds need none of this: `places` is in `config::LAYERS` and
//! `load::run` calls `load::places`, so `minimap all` produces labels itself.

use std::time::Instant;

use duckdb::Connection;
use minimap::config::{self, MAXZOOM, MINZOOM};
use minimap::{bake, load};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let con = config::connect(false)?;
    if applied(&con)? {
        config::log("places are already merged into tiles -- nothing to do");
        return Ok(());
    }

    // Every extract actually on disk. France arrives three times over (its own
    // file plus the two legacy regions), which the dedup in `load::places`
    // absorbs; scanning it twice more is cheaper than maintaining a list of
    // which regions this archive was built from.
    let regions: Vec<String> = config::available_regions()
        .into_iter()
        .filter(|r| config::pbf_path(r).exists())
        .collect();
    config::log(format!("scanning {} extracts for places", regions.len()));

    let have: i64 = con.query_row(
        "SELECT count(*) FROM features WHERE layer = 'places'",
        [],
        |r| r.get(0),
    )?;
    if have > 0 {
        config::log(format!(
            "{} place labels already loaded",
            config::commas(have as u64)
        ));
    } else {
        load::places(&con, &regions)?;
    }

    bake_places(&con)?;
    splice(&con)?;

    let (n, bytes): (i64, i64) = con.query_row(
        "SELECT count(*), sum(octet_length(data)) FROM tiles",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    config::log(format!(
        "{} tiles, {:.2} GB -- now re-run `minimap export`",
        config::commas(n as u64),
        bytes as f64 / 1e9
    ));
    Ok(())
}

fn applied(con: &Connection) -> Result<bool, Box<dyn std::error::Error>> {
    let marker: i64 = con.query_row(
        "SELECT count(*) FROM duckdb_tables() WHERE table_name = 'places_merged'",
        [],
        |r| r.get(0),
    )?;
    Ok(marker > 0)
}

/// Points, so there is no geometry to speak of and no banding needed: the whole
/// layer is a single statement per zoom.
fn bake_places(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    con.execute_batch(
        "CREATE TABLE IF NOT EXISTS place_layers
             (z UTINYINT, x UINTEGER, y UINTEGER, layer VARCHAR, data BLOB);
         CREATE TABLE IF NOT EXISTS place_done (z UTINYINT, band UBIGINT)",
    )?;
    for z in MINZOOM..=MAXZOOM {
        let done: i64 = con.query_row(
            &format!("SELECT count(*) FROM place_done WHERE z = {z}"),
            [],
            |r| r.get(0),
        )?;
        if done > 0 {
            continue;
        }
        let t0 = Instant::now();
        con.execute_batch(&format!(
            "BEGIN TRANSACTION;\n{}\n;INSERT INTO place_done VALUES ({z}, 0);\nCOMMIT;",
            bake::band_sql(z, "places", 0, 1u64 << z, "place_layers")
        ))?;
        let made: i64 =
            con.query_row("SELECT count(*) FROM place_layers WHERE z = ?", [z], |r| {
                r.get(0)
            })?;
        config::timed(
            format!("z{z}: {} label tiles", config::commas(made as u64)),
            t0,
        );
    }
    Ok(())
}

/// `existing || places`, because labels draw over everything.
fn splice(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    con.execute_batch(
        "CREATE TABLE IF NOT EXISTS tiles_places
             (z UTINYINT, x UINTEGER, y UINTEGER, data BLOB);
         CREATE TABLE IF NOT EXISTS psplice_done (z UTINYINT, band UBIGINT)",
    )?;
    let t0 = Instant::now();
    for z in MINZOOM..=MAXZOOM {
        let all = bake::bake_bands(z);
        let n = all.len();
        for (s, &(lo, hi)) in all.iter().enumerate() {
            let done: i64 = con.query_row(
                &format!("SELECT count(*) FROM psplice_done WHERE z = {z} AND band = {s}"),
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
                INSERT INTO tiles_places
                SELECT z, x, y,
                       COALESCE(t.data, ''::BLOB) || COALESCE(p.data, ''::BLOB)
                FROM (SELECT z, x, y, data FROM tiles
                      WHERE z = {z} AND x >= {lo} AND x < {hi}) t
                FULL OUTER JOIN
                     (SELECT z, x, y, data FROM place_layers
                      WHERE z = {z} AND x >= {lo} AND x < {hi}) p
                USING (z, x, y);
                INSERT INTO psplice_done VALUES ({z}, {s});
                COMMIT;
                "#
            ))?;
            if n > 1 {
                config::timed(format!("z{z} splice band {}/{n}", s + 1), t1);
            }
        }
    }
    con.execute_batch(
        "BEGIN TRANSACTION;
         DROP TABLE tiles;
         ALTER TABLE tiles_places RENAME TO tiles;
         DROP TABLE place_layers;
         DROP TABLE place_done;
         DROP TABLE psplice_done;
         CREATE INDEX tiles_zxy ON tiles (z, x, y);
         CREATE TABLE places_merged (tiles BIGINT);
         INSERT INTO places_merged SELECT count(*) FROM tiles;
         COMMIT;",
    )?;
    con.execute_batch("CHECKPOINT")?;
    config::timed("labels spliced into tiles", t0);
    Ok(())
}
