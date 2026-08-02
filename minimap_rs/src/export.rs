//! `tile_layers` -> one PMTiles archive per layer, ready to ship to the server.
//!
//! The archives are the deliverable: the build database stays on the build
//! machine.
//!
//! **One per layer, not one for the map.** PMTiles has no concept of a layer --
//! it is `(z, x, y) -> blob` -- so this is the format's own grain, and the
//! single multi-layer archive it replaced was the thing that needed a trick:
//! concatenating per-layer protobufs because `Tile` is `repeated Layer layers`.
//! Splitting them buys three things. The merge that produced that one blob is
//! gone, and with it the statement whose obvious spelling held all 22 GB of
//! Europe at once. Each layer carries its own zoom rungs, so `land` stopping at
//! z12 while `buildings` starts at z15 needs no special case anywhere. And a
//! change to one layer rebuilds and re-fetches only that layer, because each
//! archive has its own etag.
//!
//! The price is a request per layer per tile instead of one, and about ten
//! percent more bytes from gzipping each layer separately. PMTiles is used as the on-disk format even though we serve it
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

use duckdb::Connection;
use pmtiles::{Compression, Compressor, PmTilesWriter, PmtResult, TileCoord, TileId, TileType};

use crate::config::Config;
use crate::tuning::LAYERS;
use crate::progress::{self, Step};

type Error = Box<dyn std::error::Error>;

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

pub fn run(cfg: &Config, con: &Connection) -> Result<(), Error> {
    let (west, south, east, north): (f64, f64, f64, f64) = con
        .query_row("SELECT west, south, east, north FROM meta", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|_| "no `meta` table -- run `make bake` first")?;

    let dir = cfg.tiles_dir();
    std::fs::create_dir_all(&dir)?;
    let step = Step::start("export", format!("one archive per layer -> {}", dir.display()));

    // Only the layers that actually produced tiles. A layer with nothing in it
    // gets no archive at all, which is how the viewer learns it is absent --
    // there is no empty-archive case to handle anywhere downstream.
    let mut present: Vec<String> = Vec::new();
    {
        let mut stmt = con.prepare("SELECT DISTINCT layer FROM tile_layers")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            present.push(row?);
        }
    }
    let layers: Vec<&str> = LAYERS
        .iter()
        .copied()
        .filter(|l| present.iter().any(|p| p == l))
        .collect();
    if layers.is_empty() {
        return Err("no tiles to export -- run `make bake` first".into());
    }

    // Stale archives from a build with more layers than this one would otherwise
    // sit in the directory and be served.
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".pmtiles") {
            if !layers.iter().any(|l| *l == stem) {
                progress::line(format!("removing stale {name}"));
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    progress::begin(total_bytes(con)? as f64);
    let mut totals = (0u64, 0u64, 0u64); // tiles, raw, archive
    for (i, layer) in layers.iter().enumerate() {
        let prefix = progress::item(i, layers.len(), layer);
        let (tiles, raw, size) = one(cfg, con, layer, (west, south, east, north))?;
        totals = (totals.0 + tiles, totals.1 + raw, totals.2 + size);
        progress::line(format!(
            "{prefix} {:>9} tiles  {:>10} -> {:>10}",
            progress::commas(tiles),
            progress::bytes(raw),
            progress::bytes(size)
        ));
    }
    progress::end();

    println!();
    progress::line(format!(
        "{} tiles across {} archives, {} -> {} ({:.0}%, gzip -9 + Hilbert order)",
        progress::commas(totals.0),
        layers.len(),
        progress::bytes(totals.1),
        progress::bytes(totals.2),
        100.0 * totals.2 as f64 / totals.1.max(1) as f64
    ));
    progress::line(format!("archives: {}/", dir.display()));
    progress::line("serve it:  make serve");
    step.done();
    Ok(())
}

fn total_bytes(con: &Connection) -> Result<i64, Error> {
    Ok(con.query_row(
        "SELECT COALESCE(sum(octet_length(data)), 0) FROM tile_layers",
        [],
        |r| r.get(0),
    )?)
}

/// One layer's archive. Returns (tiles, raw bytes, archive bytes).
fn one(
    cfg: &Config,
    con: &Connection,
    layer: &str,
    bounds: (f64, f64, f64, f64),
) -> Result<(u64, u64, u64), Error> {
    let (west, south, east, north) = bounds;

    // The rungs this layer actually has. They are a property of the layer, not
    // of the build -- `land` stops where the background cap put it, `buildings`
    // start where they first become worth drawing -- and the viewer reads them
    // back out of the archive rather than being told separately.
    let mut rungs: Vec<u8> = Vec::new();
    {
        let mut stmt =
            con.prepare("SELECT DISTINCT z FROM tile_layers WHERE layer = ? ORDER BY z")?;
        let rows = stmt.query_map([layer], |r| r.get::<_, u8>(0))?;
        for row in rows {
            rungs.push(row?);
        }
    }
    let (minzoom, maxzoom) = (rungs[0], *rungs.last().expect("non-empty"));

    // PMTiles wants tiles in ascending TileId, which is Hilbert order and has no
    // cheap SQL spelling. Rather than pull every blob into memory to sort them
    // -- fine for a region, 20 GB for Europe -- compute the ids here, hand them
    // back to DuckDB, and let it do the sort it is good at.
    let mut stmt = con.prepare("SELECT z, x, y FROM tile_layers WHERE layer = ?")?;
    let coords: Vec<(u8, u32, u32)> = stmt
        .query_map([layer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
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

    // Written beside the archive and renamed onto it only once it is complete.
    // Creating the archive in place truncates it before the first tile is
    // written, which loses the last good build if this run fails -- and takes
    // out any running backend with it, since the file it mmapped just became
    // shorter than the pages it is reading. Renaming leaves that mmap pointing
    // at the old inode, which stays alive and correct until the server restarts.
    let out = cfg.layer_archive(layer);
    let tmp = out.with_extension("pmtiles.new");
    let rung_list = rungs
        .iter()
        .map(|z| z.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .tile_codec(Gzip9)
        .min_zoom(minzoom)
        .max_zoom(maxzoom)
        .bounds(west, south, east, north)
        .center_zoom(minzoom)
        .center((west + east) / 2.0, (south + north) / 2.0)
        .metadata(&format!(
            r#"{{"attribution":"© OpenStreetMap contributors","layer":"{layer}","rungs":[{rung_list}]}}"#
        ))
        .create(File::create(&tmp)?)?;

    let mut stmt = con.prepare(
        "SELECT t.z, t.x, t.y, t.data
         FROM tile_layers t JOIN tile_order o USING (z, x, y)
         WHERE t.layer = ?
         ORDER BY o.id",
    )?;
    let mut rows = stmt.query([layer])?;
    let (mut count, mut raw_bytes) = (0u64, 0u64);
    while let Some(row) = rows.next()? {
        let (z, x, y): (u8, u32, u32) = (row.get(0)?, row.get(1)?, row.get(2)?);
        let data: Vec<u8> = row.get(3)?;
        raw_bytes += data.len() as u64;
        progress::at(format!("{layer} z{z}"));
        progress::tick(data.len() as f64);
        writer.add_tile(TileCoord::new(z, x, y)?, &data)?;
        count += 1;
    }
    writer.finalize()?;
    std::fs::rename(&tmp, &out)?;
    con.execute_batch("DROP TABLE tile_order")?;

    Ok((count, raw_bytes, std::fs::metadata(&out)?.len()))
}
