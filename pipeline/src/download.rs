//! Fetching Geofabrik extracts.
//!
//! `download` handles the handful of regions in `config::SOURCES`. Getting all
//! 49 European country extracts is ./fetch-europe.sh's job instead, because
//! pulling 31.7 GB politely off a free service wants resumption, a retry policy
//! and a concurrency limit that curl already has and this does not.
//!
//! What the script cannot work out for itself is *which* extracts, so
//! [`europe_urls`] answers that: the exclusion list is pipeline knowledge and
//! belongs next to the rest of it rather than in a shell heredoc.

use std::io::{Read, Write};
use std::time::Instant;

use crate::config;

/// Geofabrik's machine-readable list of everything it publishes.
const INDEX_URL: &str = "https://download.geofabrik.de/index-v1-nogeom.json";

/// Aggregates that overlap their own siblings. Taking them as well would
/// download gigabytes twice and double-count features at load time:
///   alps / dach          span several countries
///   britain-and-ireland  = great-britain + ireland-and-northern-ireland
///   united-kingdom       overlaps great-britain and northern ireland
const EUROPE_SKIP: [&str; 4] = ["alps", "dach", "britain-and-ireland", "united-kingdom"];

/// One `.osm.pbf` URL per line, for every child of `europe` worth having.
///
/// Discovered from the index rather than hardcoded, so a region Geofabrik adds
/// later is picked up without anyone noticing it needs to be.
pub fn europe_urls() -> Result<(), Box<dyn std::error::Error>> {
    let body = ureq::get(INDEX_URL).call()?.into_body().read_to_string()?;
    let index: serde_json::Value = serde_json::from_str(&body)?;
    let features = index["features"].as_array().ok_or("no features in Geofabrik index")?;

    let mut urls: Vec<&str> = features
        .iter()
        .map(|f| &f["properties"])
        .filter(|p| p["parent"] == "europe")
        .filter(|p| !EUROPE_SKIP.contains(&p["id"].as_str().unwrap_or_default()))
        .filter_map(|p| p["urls"]["pbf"].as_str())
        .collect();
    urls.sort_unstable();
    for url in urls {
        println!("{url}");
    }
    Ok(())
}

pub fn run(regions: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(config::data())?;
    for region in regions {
        // Ask where the extract would be *read* from, not where this step would
        // write it: `france` and `europe` are in SOURCES and also fetched by
        // ./fetch-europe.sh, and checking only the flat path re-downloaded 5 GB
        // of France that was already sitting in data/countries/.
        let dest = config::pbf_path(region);
        if let Ok(meta) = std::fs::metadata(&dest) {
            if meta.len() > 1_000_000 {
                config::log(format!(
                    "{} already present ({:.0} MB)",
                    dest.file_name().unwrap_or(dest.as_os_str()).to_string_lossy(),
                    meta.len() as f64 / 1e6
                ));
                continue;
            }
        }
        let Some((_, url)) = config::SOURCES.iter().find(|(n, _)| n == region) else {
            // Country extracts arrive via fetch-europe.sh; nothing to do here.
            config::log(format!("{region}: not a downloadable source, skipping"));
            continue;
        };

        let t0 = Instant::now();
        config::log(format!("downloading {region} ..."));
        let response = ureq::get(*url).call()?;
        let total: u64 = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let mut reader = response.into_body().into_reader();
        let mut file = std::fs::File::create(&dest)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        // Reprinting per 1 MB chunk is fine on a terminal, where `\r` overwrites
        // the line, and 25 MB of log when stdout is a file. Once per percent is
        // enough either way.
        let mut shown = u64::MAX;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            if total > 0 && done * 100 / total != shown {
                shown = done * 100 / total;
                print!("\r    {:6.0} / {:.0} MB", done as f64 / 1e6, total as f64 / 1e6);
                std::io::stdout().flush()?;
            }
        }
        println!();
        config::timed(format!("{}.osm.pbf done", region), t0);
    }
    Ok(())
}
