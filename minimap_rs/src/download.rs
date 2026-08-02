//! Fetching what the build starts from: OSM extracts, and the coastline.
//!
//! This used to be two things — a Rust step that knew four hardcoded URLs, and
//! a shell script that used curl and xargs to get the other forty-nine. The
//! script existed because the Rust step could not resume, retry or run more
//! than one transfer at a time. Those are ~150 lines, so it does them now and
//! there is one way to fetch an extract.
//!
//! What matters here is that it is safe to interrupt and safe to re-run. These
//! are 31 GB off a free service: a download that starts over from zero because
//! a laptop slept is rude to Geofabrik and expensive for us.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::progress::{self, Step};
use crate::tuning;

type Error = Box<dyn std::error::Error>;

/// A transfer we intend to make.
struct Want {
    label: String,
    url: String,
    dest: PathBuf,
}

pub fn run(cfg: &Config, names: &[String], land: bool, europe: bool) -> Result<(), Error> {
    cfg.prepare()?;

    let mut wants: Vec<Want> = Vec::new();
    let mut adopted = 0usize;
    if !names.is_empty() || europe {
        let index = Index::load(cfg)?;
        let mut ids: Vec<String> = names.to_vec();
        if europe {
            ids.extend(index.europe_children());
        }
        ids.sort();
        ids.dedup();
        let elsewhere = cfg.extracts();
        for id in ids {
            let dest = cfg.pbf.join(format!("{id}.osm.pbf"));
            // An extract already under data/ but not where this layout expects
            // it -- the older data/countries/ tree, or a file dropped in by
            // hand. Link it into place rather than spend an hour fetching 5 GB
            // that is sitting right there. A hard link, not a move: the file
            // the user put somewhere stays where they put it.
            if !dest.exists() {
                if let Some(found) = elsewhere.iter().find(|r| r.name == id) {
                    match adopt(&found.path, &dest) {
                        Ok(how) => {
                            println!("    {id:<18} {how} {}", found.path.display());
                            adopted += 1;
                            continue;
                        }
                        Err(e) => progress::warn(format!("{id}: {e}, downloading instead")),
                    }
                }
            }
            let url = index.pbf_url(&id)?;
            wants.push(Want {
                dest,
                label: id,
                url,
            });
        }
    }
    // The coastline, which every region needs and no region provides. See
    // tuning::LAND_URL for why the sea comes from a separate download.
    if land {
        wants.push(Want {
            label: "land polygons".into(),
            url: tuning::LAND_URL.into(),
            dest: cfg.land_zip(),
        });
    }
    if wants.is_empty() {
        // Nothing to fetch is only an error if there was also nothing to do:
        // an `all` whose extracts are already here must not fail the build.
        if adopted > 0 {
            return Ok(());
        }
        return Err("nothing to download -- name a region, or pass --land / --europe".into());
    }

    let step = Step::start(
        "download",
        format!(
            "{} file{} -> {}",
            wants.len(),
            if wants.len() == 1 { "" } else { "s" },
            cfg.pbf.display()
        ),
    );

    // One transfer draws its own byte bar. Several would fight over the line,
    // so instead they share one bar counting all of their bytes together --
    // which is the only way `make europe` can say anything about how long 31 GB
    // is going to take. Sizes come from a round of concurrent HEADs, which is
    // 49 cheap requests against hours of transfer.
    let solo = wants.len() == 1;
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let total = wants.len();
    let finished = AtomicUsize::new(0);

    if !solo {
        let sizes = Mutex::new(0u64);
        let probe = AtomicUsize::new(0);
        progress::line("asking how big they are ...");
        std::thread::scope(|scope| {
            for _ in 0..cfg.jobs.clamp(1, wants.len()) {
                scope.spawn(|| loop {
                    let i = probe.fetch_add(1, Ordering::Relaxed);
                    let Some(want) = wants.get(i) else { return };
                    let remote = content_length(&want.url).ok().flatten().unwrap_or(0);
                    let have = std::fs::metadata(&want.dest).map(|m| m.len()).unwrap_or(0);
                    *sizes.lock().unwrap() += remote.saturating_sub(have.min(remote));
                });
            }
        });
        let outstanding = sizes.into_inner().unwrap();
        progress::begin(outstanding as f64);
        progress::line(format!("{} still to fetch", progress::bytes(outstanding)));
    }

    std::thread::scope(|scope| {
        for _ in 0..cfg.jobs.clamp(1, wants.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(want) = wants.get(i) else { return };
                let prefix = progress::item(i, total, &want.label);
                if !solo {
                    progress::at(format!(
                        "{}/{total} done, fetching {}",
                        finished.load(Ordering::Relaxed),
                        want.label
                    ));
                }
                match fetch(want, solo, &prefix) {
                    Ok(_) => {
                        finished.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        progress::warn(format!("{}: {e}", want.label));
                        failures.lock().unwrap().push(want.label.clone());
                    }
                }
            });
        }
    });

    let failures = failures.into_inner().unwrap();
    if !failures.is_empty() {
        return Err(format!(
            "{} of {total} failed: {} -- re-run to resume where they stopped",
            failures.len(),
            failures.join(", ")
        )
        .into());
    }
    step.done();
    Ok(())
}

/// Put an extract that is already on disk where this layout expects it.
///
/// Hard link first, so the bytes never move and neither name is the "real"
/// one — deleting either leaves the other intact. Falling back to a rename
/// covers a `data/` split across filesystems, which is unusual but is exactly
/// the case where copying 5 GB would be the wrong answer.
fn adopt(from: &Path, to: &Path) -> Result<&'static str, Error> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => Ok("link  "),
        Err(_) => {
            std::fs::rename(from, to)?;
            Ok("moved ")
        }
    }
}

/// Attempts before giving up on one file. Each one resumes rather than
/// restarts, so five attempts is five chances at the remaining bytes.
const ATTEMPTS: u32 = 5;

fn fetch(want: &Want, bar: bool, prefix: &str) -> Result<(), Error> {
    let remote = content_length(&want.url)?;
    let have = std::fs::metadata(&want.dest).map(|m| m.len()).unwrap_or(0);

    // Complete already. Compared against the server's own length rather than
    // "the file exists", because the file existing is exactly what a killed
    // transfer leaves behind.
    if let Some(remote) = remote {
        if have == remote && remote > 0 {
            println!("    {prefix} have  {:>9}", progress::bytes(have));
            return Ok(());
        }
    }

    let mut last: Option<Error> = None;
    for attempt in 1..=ATTEMPTS {
        match attempt_fetch(want, bar, prefix, remote) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < ATTEMPTS {
                    progress::warn(format!(
                        "{}: {e} (attempt {attempt}/{ATTEMPTS}, retrying)",
                        want.label
                    ));
                    std::thread::sleep(Duration::from_secs(2 * u64::from(attempt)));
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| "download failed".into()))
}

fn attempt_fetch(want: &Want, bar: bool, prefix: &str, remote: Option<u64>) -> Result<(), Error> {
    let have = std::fs::metadata(&want.dest).map(|m| m.len()).unwrap_or(0);
    let t0 = Instant::now();

    // Ask to continue rather than to start. A server that will not (no 206)
    // gets us a 200 and a full body, which the `resumed` check below turns back
    // into a truncating write, so a missing Range never corrupts a file.
    let mut request = ureq::get(&want.url);
    if have > 0 {
        request = request.header("Range", &format!("bytes={have}-"));
    }
    let response = request.call()?;
    let resumed = response.status() == 206;
    let body_len: Option<u64> = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let total = remote.or_else(|| body_len.map(|n| n + if resumed { have } else { 0 }));

    if let Some(parent) = want.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = if resumed {
        std::fs::OpenOptions::new().append(true).open(&want.dest)?
    } else {
        std::fs::File::create(&want.dest)?
    };
    let done_before = if resumed { have } else { 0 };
    if !bar {
        println!(
            "    {prefix} {} {}",
            if resumed { "resume" } else { "get   " },
            total.map(progress::bytes).unwrap_or_default()
        );
    }

    let mut reader = response.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut meter = bar.then(|| {
        let mut m = progress::Bytes::new(want.label.clone(), total.unwrap_or(0));
        m.add(done_before);
        m
    });
    let mut done = done_before;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        match meter.as_mut() {
            Some(m) => m.add(n as u64),
            // No per-file meter means the shared one is running; feed it the
            // bytes directly so `make europe` has a bar and an ETA.
            None => progress::tick(n as f64),
        }
    }
    file.flush()?;
    drop(file);

    // A transfer cut short mid-stream ends cleanly at EOF and looks like
    // success. Only the length says otherwise, so check it and let the retry
    // loop resume from wherever it actually stopped.
    if let Some(total) = total {
        if done < total {
            return Err(format!(
                "truncated at {} of {}",
                progress::bytes(done),
                progress::bytes(total)
            )
            .into());
        }
    }

    match meter {
        Some(m) => m.finish(),
        None => println!(
            "    {prefix} ok    {:>9} in {}",
            progress::bytes(done),
            progress::secs(t0.elapsed())
        ),
    }
    Ok(())
}

fn content_length(url: &str) -> Result<Option<u64>, Error> {
    let response = ureq::head(url).call()?;
    Ok(response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok()))
}

// --- Geofabrik's catalogue -------------------------------------------------

/// Geofabrik's own index of everything it publishes, cached in `data/`.
///
/// Resolving names through it means there is no table of regions to maintain
/// here: whatever works on their download page works as a `REGIONS=` entry, and
/// a region they add later needs no change. It also means a typo is caught
/// before the download rather than as a 404 halfway through a `make all`.
pub struct Index {
    entries: Vec<Entry>,
}

struct Entry {
    id: String,
    parent: Option<String>,
    pbf: Option<String>,
}

impl Index {
    pub fn load(cfg: &Config) -> Result<Index, Error> {
        let path = cfg.index_json();
        // ~3 MB, and it changes about as often as Geofabrik adds a country.
        let body = match std::fs::read_to_string(&path) {
            Ok(body) if !body.is_empty() => body,
            _ => {
                let body = ureq::get(tuning::GEOFABRIK_INDEX)
                    .call()?
                    .into_body()
                    .read_to_string()?;
                std::fs::create_dir_all(&cfg.pbf)?;
                std::fs::write(&path, &body)?;
                body
            }
        };
        let json: serde_json::Value = serde_json::from_str(&body)?;
        let features = json["features"]
            .as_array()
            .ok_or("no features in the Geofabrik index")?;
        let entries = features
            .iter()
            .map(|f| &f["properties"])
            .filter_map(|p| {
                Some(Entry {
                    id: p["id"].as_str()?.to_string(),
                    parent: p["parent"].as_str().map(str::to_string),
                    pbf: p["urls"]["pbf"].as_str().map(str::to_string),
                })
            })
            .collect();
        Ok(Index { entries })
    }

    pub fn pbf_url(&self, id: &str) -> Result<String, Error> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| -> Error {
                let near: Vec<&str> = self
                    .entries
                    .iter()
                    .filter(|e| e.id.contains(id) || id.contains(&e.id))
                    .map(|e| e.id.as_str())
                    .take(8)
                    .collect();
                if near.is_empty() {
                    format!("unknown region {id:?} -- `make regions` lists them").into()
                } else {
                    format!("unknown region {id:?} -- did you mean: {}", near.join(", ")).into()
                }
            })?;
        entry
            .pbf
            .clone()
            .ok_or_else(|| format!("{id} has no .osm.pbf").into())
    }

    /// Every child of `europe` worth having; see [`tuning::EUROPE_SKIP`].
    pub fn europe_children(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.parent.as_deref() == Some("europe"))
            .filter(|e| !tuning::EUROPE_SKIP.contains(&e.id.as_str()))
            .filter(|e| e.pbf.is_some())
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        ids
    }
}

/// `minimap regions` — what can go in `REGIONS=`, and what is already here.
pub fn list(cfg: &Config) -> Result<(), Error> {
    let index = Index::load(cfg)?;
    let have: Vec<String> = cfg.extracts().into_iter().map(|r| r.name).collect();
    let children = index.europe_children();

    println!("European extracts ({} of them):\n", children.len());
    for chunk in children.chunks(4) {
        let cells: Vec<String> = chunk
            .iter()
            .map(|id| {
                let mark = if have.contains(id) { "x" } else { " " };
                format!("[{mark}] {id:<26}")
            })
            .collect();
        println!("  {}", cells.join(""));
    }
    println!("\n  [x] = already in {}", cfg.pbf.display());
    println!("\nSub-regions work too (picardie, bayern, ...) -- any id from");
    println!("https://download.geofabrik.de/ is a valid REGIONS= entry.");

    let extra: Vec<&String> = have.iter().filter(|h| !children.contains(h)).collect();
    if !extra.is_empty() {
        println!("\nalso downloaded: {}",
            extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

/// Where a named region's extract would be written, for callers that need the
/// path before the file exists.
pub fn dest(cfg: &Config, id: &str) -> PathBuf {
    cfg.pbf.join(format!("{id}.osm.pbf"))
}

/// Whether a path looks like a complete download. Used only for reporting.
pub fn present(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 1_000_000).unwrap_or(false)
}
