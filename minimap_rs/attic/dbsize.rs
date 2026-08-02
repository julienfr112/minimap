//! One-off probe: where do the database's bytes go?
use duckdb::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("usage: dbsize <db path>");
    let con = Connection::open_with_flags(
        &path,
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    con.execute_batch("INSTALL spatial; LOAD spatial;")?;

    println!("--- PRAGMA database_size ---");
    let mut stmt = con.prepare("SELECT database_size, block_size, total_blocks, used_blocks, free_blocks, wal_size FROM pragma_database_size()")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let db_size: String = r.get(0)?;
        let block: i64 = r.get(1)?;
        let total: i64 = r.get(2)?;
        let used: i64 = r.get(3)?;
        let free: i64 = r.get(4)?;
        let wal: String = r.get(5)?;
        println!("size {db_size}  block {block}  blocks total {total} used {used} free {free}  wal {wal}");
        println!("used  {:.1} GB", used as f64 * block as f64 / 1e9);
        println!("free  {:.1} GB", free as f64 * block as f64 / 1e9);
    }

    println!("--- per-column compressed footprint (features) ---");
    let mut stmt = con.prepare(
        "SELECT column_name,
                round(count(DISTINCT block_id) * 262144 / 1e9, 2) AS gb,
                list(DISTINCT compression) AS comp
         FROM pragma_storage_info('features')
         WHERE segment_type NOT IN ('VALIDITY')
         GROUP BY column_name ORDER BY gb DESC",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let col: String = r.get(0)?;
        let gb: f64 = r.get(1)?;
        let comp: String = r
            .get::<_, duckdb::types::Value>(2)
            .map(|v| format!("{v:?}"))?;
        println!("{col:10} {gb:8.2} GB  {comp}");
    }

    println!("--- sampled logical geometry size (WKB bytes/feature) ---");
    let mut stmt = con.prepare(
        "SELECT layer, count(*),
                round(avg(octet_length(ST_AsWKB(geom))), 0),
                round(avg((octet_length(ST_AsWKB(geom)) - 9) / 16), 0)
         FROM (SELECT layer, geom FROM features TABLESAMPLE system(1%))
         GROUP BY layer ORDER BY 2 DESC",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let layer: String = r.get(0)?;
        let n: i64 = r.get(1)?;
        let avg: f64 = r.get(2)?;
        let pts: f64 = r.get(3)?;
        println!("{layer:10} sampled {n:9}  avg {avg:6.0} B  (~{pts:.0} vertices)");
    }
    Ok(())
}
