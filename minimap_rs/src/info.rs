//! Where the build got to — the answer to "is it worth waiting, or did it die?"
//!
//! Reads whatever exists and says so; nothing here is an error, because "the
//! database is not there yet" is the most useful thing this can report.

use crate::config::Config;
use crate::progress;

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

    println!("\nduckdb {}", cfg.duckdb.display());
    let db = cfg.db();
    let Ok(meta) = std::fs::metadata(&db) else {
        println!("       no database yet -- run `make load`");
        return archive(cfg);
    };
    println!(
        "       {} {}",
        db.file_name().unwrap_or_default().to_string_lossy(),
        progress::bytes(meta.len())
    );

    let con = cfg.connect(true)?;

    if let Ok((lo, hi)) = con.query_row("SELECT minzoom, maxzoom FROM build_meta", [], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    }) {
        println!("       loaded for z{lo}..{hi}");
    }

    // Per-layer feature counts say which classification rules actually fired,
    // which is the first thing to check when a map comes out empty.
    if let Ok(mut stmt) =
        con.prepare("SELECT layer, count(*) FROM features GROUP BY layer ORDER BY 2 DESC")
    {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        {
            let rows: Vec<_> = rows.flatten().collect();
            if !rows.is_empty() {
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
        }
    }

    if let Ok(mut stmt) =
        con.prepare("SELECT z, count(*), sum(octet_length(data)) FROM tiles GROUP BY z ORDER BY z")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }) {
            let rows: Vec<_> = rows.flatten().collect();
            if !rows.is_empty() {
                println!("\n       tiles");
                for (z, n, b) in &rows {
                    println!(
                        "         z{z:<11} {:>14}  {:>10}",
                        progress::commas(*n as u64),
                        progress::bytes(*b as u64)
                    );
                }
                println!(
                    "         {:<12} {:>14}  {:>10}",
                    "total",
                    progress::commas(rows.iter().map(|r| r.1).sum::<i64>() as u64),
                    progress::bytes(rows.iter().map(|r| r.2).sum::<i64>() as u64)
                );
            }
        }
    }

    if let Ok((rungs, bg, w, s, e, n)) = con.query_row(
        "SELECT rungs, background_maxzoom, west, south, east, north FROM meta",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, f64>(5)?,
            ))
        },
    ) {
        println!("\n       rungs z{rungs}, background to z{bg}");
        println!("       bounds [{w:.3}, {s:.3}, {e:.3}, {n:.3}]");
    }

    archive(cfg)
}

fn archive(cfg: &Config) -> Result<(), Error> {
    let dir = cfg.tiles_dir();
    let mut found: Vec<(String, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(layer) = name.strip_suffix(".pmtiles") {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                found.push((layer.to_string(), len));
            }
        }
    }
    if found.is_empty() {
        println!("\n       no archives yet -- run `make export`");
        return Ok(());
    }
    // Draw order, so the listing reads the way the map is painted.
    found.sort_by_key(|(layer, _)| {
        crate::tuning::LAYERS.iter().position(|l| l == layer).unwrap_or(usize::MAX)
    });
    println!("\n       {}/  <- the deliverable", dir.display());
    for (layer, len) in &found {
        println!("         {layer:<12} {:>10}", progress::bytes(*len));
    }
    println!(
        "         {:<12} {:>10}",
        "total",
        progress::bytes(found.iter().map(|(_, l)| l).sum())
    );
    Ok(())
}
