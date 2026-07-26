//! What is in the build database right now.

use duckdb::Connection;

use crate::config;

pub fn run(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let path = config::db();
    println!("  database: {} ({:.0} MB)", path.display(), std::fs::metadata(&path)?.len() as f64 / 1e6);

    for table in ["features", "tiles"] {
        match con.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0)) {
            Ok(n) => println!("  {table}: {} rows", config::commas(n as u64)),
            Err(_) => println!("  {table}: missing"),
        }
    }

    if let Ok((minzoom, maxzoom, west, south, east, north)) =
        con.query_row("SELECT minzoom, maxzoom, west, south, east, north FROM meta", [], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, f64>(5)?,
            ))
        })
    {
        println!("  meta: z{minzoom}..{maxzoom}  bounds [{west:.4}, {south:.4}, {east:.4}, {north:.4}]");
    }

    if let Ok(mut stmt) = con.prepare(
        "SELECT z, count(*), sum(octet_length(data)) FROM tiles GROUP BY z ORDER BY z",
    ) {
        println!("\n  tiles per zoom:");
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (z, n, bytes) = row?;
            println!("    z{z:<3} {:>9} tiles  {:>7.1} MB", config::commas(n as u64), bytes as f64 / 1e6);
        }
    }
    Ok(())
}
