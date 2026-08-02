//! Ad-hoc read-only SQL against the database: there is no duckdb CLI on this
//! machine, and the bundled build is the only binary that matches the file.
//!
//!   cargo run --release --example q -- "SELECT ..."
use duckdb::types::Value;
use duckdb::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args().nth(1).expect("usage: q <sql>");
    let con = Connection::open_with_flags(
        crate::config_db(),
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    con.execute_batch("INSTALL spatial; LOAD spatial;")?;
    let mut stmt = con.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let mut out = Vec::new();
        for i in 0.. {
            match r.get::<_, Value>(i) {
                Ok(v) => out.push(format!("{v:?}")),
                Err(_) => break,
            }
        }
        println!("{}", out.join("\t"));
    }
    Ok(())
}

fn config_db() -> std::path::PathBuf {
    std::env::var("MINIMAP_DB")
        .map(Into::into)
        .unwrap_or_else(|_| "/home/julien/Projets/minimap/minimap.duckdb".into())
}
