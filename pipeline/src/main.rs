//! minimap — build a vector-tile map from OpenStreetMap.
//!
//! ```text
//! download -> load -> bake -> export
//!   download   fetch the Geofabrik .osm.pbf extracts
//!   load       parse the PBFs into a DuckDB `features` table (EPSG:3857)
//!   bake       clip/simplify/encode every feature into MVT tiles
//!   export     pack the tiles into one PMTiles archive to ship to the server
//! ```
//!
//! Everything geometric happens once, here, offline, in DuckDB SQL. What
//! `minimap-backend` is left with at runtime is a keyed blob lookup, so it
//! carries no geometry code, no protobuf library and no database at all — which
//! is why DuckDB is a dependency of this crate and not of the workspace.
//!
//! Usage:  minimap all          (or: download / load / bake / export / info)

mod bake;
mod config;
mod download;
mod export;
mod extract;
mod geom;
mod info;
mod load;
mod rows;

use duckdb::Connection;

/// See the note on the dependency in Cargo.toml: this is worth ~2x on the bake,
/// because it replaces the allocator DuckDB's C++ uses, not just Rust's.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const STEPS: [&str; 7] =
    ["download", "load", "bake", "export", "info", "europe-urls", "all"];

fn main() {
    if let Err(e) = run() {
        eprintln!("minimap: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut step: Option<String> = None;
    let mut regions: Vec<String> = Vec::new();
    // `--regions` takes everything after it, so it has to come last. That is
    // the one ordering rule and the usage line states it.
    let mut reading_regions = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => return usage(),
            "--regions" => reading_regions = true,
            _ if reading_regions => regions.push(arg),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}").into()),
            _ if step.is_none() => step = Some(arg),
            _ => return Err(format!("unexpected argument {arg:?}").into()),
        }
    }

    let Some(step) = step else { return usage() };
    if !STEPS.contains(&step.as_str()) {
        return Err(format!("unknown step {step:?} -- expected one of {}", STEPS.join(", ")).into());
    }
    if regions.is_empty() {
        regions = config::DEFAULT_REGIONS.iter().map(|r| r.to_string()).collect();
    }
    let known = config::available_regions();
    if let Some(bad) = regions.iter().find(|r| !known.contains(r)) {
        return Err(format!(
            "unknown region {bad:?} -- available: {}",
            known.join(", ")
        )
        .into());
    }

    // `europe-urls` writes a list a shell script pipes somewhere. Its stdout is
    // data, so it gets no banner and no timing line.
    if step == "europe-urls" {
        return download::europe_urls();
    }

    let chosen: Vec<&str> = if step == "all" {
        vec!["download", "load", "bake", "export"]
    } else {
        vec![step.as_str()]
    };

    for name in chosen {
        println!("\n=== {name} ===");
        let t0 = std::time::Instant::now();
        match name {
            "download" => download::run(&regions)?,
            "load" => load::run(&connect(false)?, &regions)?,
            "bake" => bake::run(&connect(false)?)?,
            "export" => export::run(&connect(false)?)?,
            "info" => info::run(&connect(true)?)?,
            _ => unreachable!(),
        }
        println!("=== {name} finished in {:.1}s ===", t0.elapsed().as_secs_f64());
    }

    if step == "all" || step == "export" {
        println!("\nNow serve it:  cargo run --release --bin minimap-backend");
    }
    Ok(())
}

fn connect(read_only: bool) -> Result<Connection, Box<dyn std::error::Error>> {
    let path = config::db();
    if read_only && !path.exists() {
        return Err(format!("{} does not exist yet", path.display()).into());
    }
    let con = if read_only {
        Connection::open_with_flags(&path, duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?)?
    } else {
        Connection::open(&path)?
    };
    con.execute_batch("INSTALL spatial; LOAD spatial; SET preserve_insertion_order = false;")?;
    Ok(con)
}

fn usage() -> Result<(), Box<dyn std::error::Error>> {
    println!("usage: minimap <{}> [--regions REGION ...]", STEPS.join("|"));
    println!("\navailable regions: {}", config::available_regions().join(", "));
    println!("default: {}", config::DEFAULT_REGIONS.join(" "));
    Ok(())
}
