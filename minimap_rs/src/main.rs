//! minimap — build a vector-tile map from OpenStreetMap.
//!
//! ```text
//! download -> load -> bake -> export
//!   download   fetch the .osm.pbf extracts and the coastline into data/
//!   load       parse them into a DuckDB `features` table (EPSG:3857)
//!   bake       clip/simplify/encode every feature into MVT tiles
//!   export     pack the tiles into one PMTiles archive to ship to the server
//! ```
//!
//! Everything geometric happens once, here, offline, in DuckDB SQL. What
//! `minimap-backend` is left with at runtime is a keyed blob lookup, so it
//! carries no geometry code, no protobuf library and no database at all — which
//! is why DuckDB is a dependency of this crate and not of the workspace.
//!
//! **This is not the interface you are meant to use.** The Makefile is: it
//! knows which stages are already done, keeps the logs, and cleans up. Every
//! flag below exists so that `make` can say exactly what it wants and so that
//! nothing is inferred from where the binary happens to live. Run `make` for
//! the list.

use minimap::config::Config;
use minimap::{bake, download, export, info, load, progress, sql, tuning};

/// See the note on the dependency in Cargo.toml: this is worth ~2x on the bake,
/// because it replaces the allocator DuckDB's C++ uses, not just Rust's.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

type Error = Box<dyn std::error::Error>;

const USAGE: &str = "\
usage: minimap <command> [options] [REGION ...]

commands
  download   fetch extracts (and --land) into <pbf>
  load       PBF -> DuckDB `features`
  bake       `features` -> MVT tiles
  export     tiles -> PMTiles archive
  all        load, bake, export
  info       what is in the build right now
  regions    what Geofabrik publishes, and what is already here
  sql QUERY  ask the build database something

options
  --pbf DIR       the extracts, never cleaned    [pbf]
  --duckdb DIR    the build database and spill    [duckdb]
  --pmtiles DIR   the archives, one per layer     [pmtiles]
  --memory SIZE   DuckDB budget, e.g. 8GB        [half of RAM]
  --jobs N        concurrent downloads           [3]
  --log FILE      also write durable lines here   [none]
  --land          (download) also fetch the coastline dataset
  --europe        (download) every European country extract
  -h, --help

REGION is a Geofabrik id (picardie, france, belgium). `minimap regions` lists
them. For load/bake/export, naming none means every extract under <pbf>.

The zoom rungs and the size thresholds are not flags: they are what this map is,
and they live in minimap_rs/src/tuning.rs. Editing that file makes `make` re-run
the stages that went stale.

You probably want `make` instead; it drives all of this.";

fn main() {
    if let Err(e) = run() {
        eprintln!("\nminimap: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    let mut cfg = Config::default();
    let mut command: Option<String> = None;
    let mut regions: Vec<String> = Vec::new();
    let mut land = false;
    let mut europe = false;
    let mut log: Option<std::path::PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // Every option takes its value as the next argument, so there is one
        // spelling and no `=` variant to get wrong.
        let mut value = |name: &str| -> Result<String, Error> {
            args.next()
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                println!("\nthis build bakes rungs z{}", tuning::zooms_csv());
                return Ok(());
            }
            "--pbf" => cfg.pbf = value("--pbf")?.into(),
            "--duckdb" => cfg.duckdb = value("--duckdb")?.into(),
            "--pmtiles" => cfg.pmtiles = value("--pmtiles")?.into(),
            "--memory" => cfg.memory = Some(value("--memory")?),
            "--jobs" => cfg.jobs = value("--jobs")?.parse()?,
            "--log" => log = Some(value("--log")?.into()),
            "--land" => land = true,
            "--europe" => europe = true,
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option {arg}\n\n{USAGE}").into())
            }
            _ if command.is_none() => command = Some(arg),
            _ => regions.push(arg),
        }
    }

    let Some(command) = command else {
        println!("{USAGE}");
        return Ok(());
    };
    // Before anything can be reported, so the log holds the whole run. Note
    // this is not `| tee`: a pipe would make stdout a non-terminal and switch
    // off the live progress line for the one audience it exists for.
    if let Some(path) = &log {
        progress::log_to(path)?;
    }
    match command.as_str() {
        "download" => {
            // `download` with nothing named would otherwise silently do
            // nothing, which in a Makefile looks exactly like success.
            download::run(&cfg, &regions, land || regions.is_empty() && !europe, europe)
        }
        "regions" => download::list(&cfg),
        "info" => info::run(&cfg),
        "sql" => sql::run(&cfg, &regions.join(" ")),
        "load" | "bake" | "export" | "all" => {
            cfg.prepare()?;
            let stages: &[&str] = match command.as_str() {
                "all" => &["load", "bake", "export"],
                "load" => &["load"],
                "bake" => &["bake"],
                _ => &["export"],
            };
            // One connection for the whole run. Opening it per stage is a
            // second CHECKPOINT and a second spatial-extension load, and at
            // Europe scale that is minutes.
            let con = cfg.connect(false)?;
            let t0 = std::time::Instant::now();
            for stage in stages {
                match *stage {
                    "load" => load::run(&cfg, &con, &cfg.regions(&regions)?)?,
                    "bake" => bake::run(&con)?,
                    "export" => export::run(&cfg, &con)?,
                    _ => unreachable!(),
                }
            }
            if stages.len() > 1 {
                println!("\nall done in {}", progress::secs(t0.elapsed()));
            }
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    }
}
