//! `tiles` -> a single PMTiles archive, ready to ship to the server.
//!
//! The archive is the deliverable: the build database stays on the build
//! machine. PMTiles is used as the on-disk format even though we serve it
//! ourselves, because it already provides exactly what a small,
//! memory-constrained server needs and what measurement showed matters:
//!
//!   * tiles ordered along a Hilbert curve, so the tiles of one viewport are
//!     physically adjacent -- measured 112 us -> 1.2 us for adjacent vs random
//!     cold reads, a ~90x effect that dwarfs any store choice;
//!   * a two-level directory (root capped at 16 kB) so a lookup costs 1-2 page
//!     faults instead of ~22 scattered probes through a flat index;
//!   * tiles stored already gzipped, so the server spends no CPU per request
//!     and egress drops by a third.
//!
//! Writing goes through the reference implementation rather than hand-rolled
//! spec code, so the archive is correct by construction.

use std::fs::File;
use std::io::Write;
use std::time::Instant;

use duckdb::Connection;
use pmtiles::{Compression, Compressor, PmTilesWriter, PmtResult, TileCoord, TileId, TileType};

use crate::config::{self, MINZOOM};

/// gzip at maximum effort.
///
/// The crate's built-in gzip codec is not exported and defaults to level 6.
/// Level 9 is worth spelling out here because this compression happens exactly
/// once per build and every byte it saves is saved again on every request the
/// archive ever serves -- that is the whole argument for compressing at bake
/// time instead of per response.
struct Gzip9;

impl Compressor for Gzip9 {
    fn compression(&self) -> Compression {
        Compression::Gzip
    }

    fn compress(
        &self,
        f: &mut dyn FnMut(&mut dyn Write) -> std::io::Result<()>,
        writer: &mut dyn Write,
    ) -> PmtResult<()> {
        let mut encoder = flate2::write::GzEncoder::new(writer, flate2::Compression::best());
        f(&mut encoder)?;
        encoder.finish()?;
        Ok(())
    }
}

pub fn run(con: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let (minzoom, maxzoom, west, south, east, north): (u8, u8, f64, f64, f64, f64) = con
        .query_row("SELECT minzoom, maxzoom, west, south, east, north FROM meta", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?;

    // PMTiles wants tiles in ascending TileId, which is Hilbert order and has no
    // cheap SQL spelling. Rather than pull every blob into memory to sort them
    // -- fine for a region, 20 GB for Europe -- compute the ids here, hand them
    // back to DuckDB, and let it do the sort it is good at.
    let t0 = Instant::now();
    let mut stmt = con.prepare("SELECT z, x, y FROM tiles")?;
    let coords: Vec<(u8, u32, u32)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<duckdb::Result<_>>()?;

    con.execute_batch(
        "DROP TABLE IF EXISTS tile_order;
         CREATE TABLE tile_order (z UTINYINT, x UINTEGER, y UINTEGER, id UBIGINT)",
    )?;
    {
        let mut appender = con.appender("tile_order")?;
        for &(z, x, y) in &coords {
            let id: TileId = TileCoord::new(z, x, y)?.into();
            appender.append_row(duckdb::params![z, x, y, id.value()])?;
        }
        appender.flush()?;
    }
    config::timed(format!("{} tile ids", config::commas(coords.len() as u64)), t0);

    let out = config::archive();
    let t0 = Instant::now();
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .tile_codec(Gzip9)
        .min_zoom(minzoom)
        .max_zoom(maxzoom)
        .bounds(west, south, east, north)
        .center_zoom(MINZOOM + 2)
        .center((west + east) / 2.0, (south + north) / 2.0)
        .metadata(r#"{"attribution":"© OpenStreetMap contributors"}"#)
        .create(File::create(&out)?)?;

    let mut stmt = con.prepare(
        "SELECT t.z, t.x, t.y, t.data
         FROM tiles t JOIN tile_order o USING (z, x, y)
         ORDER BY o.id",
    )?;
    let mut rows = stmt.query([])?;
    let (mut count, mut raw_bytes) = (0u64, 0u64);
    while let Some(row) = rows.next()? {
        let (z, x, y): (u8, u32, u32) = (row.get(0)?, row.get(1)?, row.get(2)?);
        let data: Vec<u8> = row.get(3)?;
        raw_bytes += data.len() as u64;
        writer.add_tile(TileCoord::new(z, x, y)?, &data)?;
        count += 1;
    }
    writer.finalize()?;
    con.execute_batch("DROP TABLE tile_order")?;

    let size = std::fs::metadata(&out)?.len();
    config::timed(
        format!("{} tiles -> {}", config::commas(count), out.file_name().unwrap().to_string_lossy()),
        t0,
    );
    config::log(format!(
        "{:.1} MB of tiles -> {:.1} MB archive ({:.0}%, gzip + Hilbert order)",
        raw_bytes as f64 / 1e6,
        size as f64 / 1e6,
        100.0 * size as f64 / raw_bytes as f64
    ));
    Ok(())
}
