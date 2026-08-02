//! Ad-hoc read-write SQL against the database. `q`'s sibling, for the times a
//! one-off has to change something rather than just look at it -- stamping a
//! marker a migration should have written, undoing a bad column.
//!
//!   cargo run --release --example qw -- "CREATE TABLE ..."
//!
//! Opened through `config::connect`, so it gets the same spatial extension and
//! memory budget as the pipeline. Nothing here is a substitute for putting the
//! change in the pipeline: if it needs doing twice, it belongs in a step.
use duckdb::types::Value;
use minimap::config;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args().nth(1).expect("usage: qw <sql>");
    let con = config::connect(false)?;
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
