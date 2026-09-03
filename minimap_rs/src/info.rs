//! Where the build got to — the answer to "is it worth waiting, or did it die?"
//!
//! Reads whatever exists and says so; nothing here is an error, because "the
//! database is not there yet" is the most useful thing this can report. Every
//! table is optional: a database that has been loaded but not baked has
//! `features` and nothing else, and that is a state, not a failure.

use std::collections::BTreeMap;

use duckdb::Connection;

use crate::config::{split_archive_name, Config};
use crate::tuning;

type Error = Box<dyn std::error::Error>;

pub fn run(cfg: &Config) -> Result<(), Error> {
    println!("pbf    {}", cfg.pbf.display());
    let extracts = cfg.extracts();
    let bytes: u64 = extracts
        .iter()
        .filter_map(|r| std::fs::metadata(&r.path).ok())
        .map(|m| m.len())
        .sum();
    println!(
        "       {} extract{}, {}{}",
        extracts.len(),
        if extracts.len() == 1 { "" } else { "s" },
        progress::bytes(bytes),
        if cfg.land_zip().exists() {
            " + coastline"
        } else {
            " (no coastline yet)"
        }
    );
    let names: Vec<&str> = extracts.iter().map(|r| r.name.as_str()).collect();
    for chunk in names.chunks(6) {
        println!("       {}", chunk.join(" "));
    }

    println!("\nbuild  {}", cfg.name);
    println!("duckdb {}", cfg.duckdb.display());
    let db = cfg.db();
    let Ok(meta) = std::fs::metadata(&db) else {
        println!(
            "       no {} yet -- run `make load`",
            db.file_name().unwrap_or_default().to_string_lossy()
        );
        return archives(cfg);
    };
    println!(
        "       {} {}",
        db.file_name().unwrap_or_default().to_string_lossy(),
        progress::bytes(meta.len())
    );

    let con = cfg.connect(true)?;
    features(&con);
    tiles(&con);
    archives(cfg)
}

/// Per-layer feature counts say which classification rules actually fired,
/// which is the first thing to check when a map comes out empty.
fn features(con: &Connection) {
    let rows = rows(
        con,
        "SELECT layer, count(*) FROM features GROUP BY layer ORDER BY 2 DESC",
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    );
    if rows.is_empty() {
        println!("       no features -- `make load` has not finished");
        return;
    }
    println!("\n       features");
    for (layer, n) in &rows {
        println!("         {layer:<12} {:>14}", progress::commas(*n as u64));
    }
    let total: i64 = rows.iter().map(|r| r.1).sum();
    println!(
        "         {:<12} {:>14}",
        "total",
        progress::commas(total as u64)
    );
}

/// What the bake produced: tiles per rung, bytes per layer, and the extent.
/// `tile_layers` is what export reads, so its rungs are the rungs the archives
/// will have -- which may differ from this binary's `tuning.rs` if it was
/// edited since, and the point of printing them is to notice that.
fn tiles(con: &Connection) {
    let by_zoom = rows(
        con,
        "SELECT z, count(DISTINCT (x, y)), sum(octet_length(data))
         FROM tile_layers GROUP BY z ORDER BY z",
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    );
    if by_zoom.is_empty() {
        println!("\n       no tiles yet -- run `make bake`");
        return;
    }
    println!("\n       tiles");
    for (z, n, b) in &by_zoom {
        println!(
            "         z{z:<11} {:>14}  {:>10}",
            progress::commas(*n as u64),
            progress::bytes(*b as u64)
        );
    }
    let by_layer = rows(
        con,
        "SELECT layer, count(*), sum(octet_length(data))
         FROM tile_layers GROUP BY layer ORDER BY 3 DESC",
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    );
    for (layer, n, b) in &by_layer {
        println!(
            "         {layer:<12} {:>14}  {:>10}",
            progress::commas(*n as u64),
            progress::bytes(*b as u64)
        );
    }
    let rungs: Vec<String> = by_zoom.iter().map(|(z, ..)| z.to_string()).collect();
    let baked = rungs.join(",");
    let wanted = tuning::zooms_csv();
    println!(
        "\n       baked rungs z{baked}{}",
        if baked == wanted {
            String::new()
        } else {
            format!("  (tuning.rs now says z{wanted} -- `make all` will re-bake)")
        }
    );
    if let Ok((w, s, e, n)) = con.query_row("SELECT west, south, east, north FROM meta", [], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
            r.get::<_, f64>(3)?,
        ))
    }) {
        println!("       bounds [{w:.3}, {s:.3}, {e:.3}, {n:.3}]");
    }
}

/// Run a query and collect what comes back; a missing table is an empty list.
fn rows<T>(
    con: &Connection,
    sql: &str,
    map: impl FnMut(&duckdb::Row<'_>) -> duckdb::Result<T>,
) -> Vec<T> {
    let Ok(mut stmt) = con.prepare(sql) else {
        return Vec::new();
    };
    match stmt.query_map([], map) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Every build in the archive directory, not only this one: the directory is
/// shared, and "which maps are there to serve" is the question.
fn archives(cfg: &Config) -> Result<(), Error> {
    let mut builds: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(&cfg.pmtiles) {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if let Some((build, layer)) = split_archive_name(&file) {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                builds
                    .entry(build.to_string())
                    .or_default()
                    .push((layer.to_string(), len));
            }
        }
    }
    if builds.is_empty() {
        println!("\n       no archives yet -- run `make export`");
        return Ok(());
    }
    println!("\n       {}/  <- the deliverable", cfg.pmtiles.display());
    for (build, layers) in &mut builds {
        // Draw order, so the listing reads the way the map is painted.
        layers.sort_by_key(|(layer, _)| {
            tuning::LAYERS
                .iter()
                .position(|l| l == layer)
                .unwrap_or(usize::MAX)
        });
        let mark = if *build == cfg.name {
            "  <- this build"
        } else {
            ""
        };
        println!("       {build}{mark}");
        for (layer, len) in layers.iter() {
            println!("         {layer:<12} {:>10}", progress::bytes(*len));
        }
        println!(
            "         {:<12} {:>10}",
            "total",
            progress::bytes(layers.iter().map(|(_, l)| l).sum())
        );
    }
    Ok(())
}
