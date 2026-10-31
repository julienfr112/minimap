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
use crate::tuning;
use progress::Step;

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
            // An extract already under pbf/ but not where this layout expects
            // it -- a subdirectory from an older layout, or a file dropped in
            // by hand. Link it into place rather than spend an hour fetching 5 GB
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
        let asked = AtomicUsize::new(0);
        progress::line("asking how big they are ...");
        std::thread::scope(|scope| {
            for _ in 0..cfg.jobs.clamp(1, wants.len()) {
                scope.spawn(|| loop {
                    let i = asked.fetch_add(1, Ordering::Relaxed);
                    let Some(want) = wants.get(i) else { return };
                    // A finished file is not fetched again; a fragment counts
                    // what it still lacks, assuming the version holds.
                    if want.dest.exists() {
                        continue;
                    }
                    let remote = probe(&want.url).ok().and_then(|r| r.len).unwrap_or(0);
                    let have = std::fs::metadata(part_of(&want.dest))
                        .map(|m| m.len())
                        .unwrap_or(0);
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
/// covers a `pbf/` split across filesystems, which is unusual but is exactly
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

/// Where a transfer writes until it is complete: `<dest>.part`. Only a finished
/// file ever has its real name, so a file under that name is never a fragment
/// -- not to `fetch`, not to make's `$(PBF)/%.osm.pbf` rule, and not to
/// `collect_pbfs`, which only takes names ending in `.osm.pbf`.
fn part_of(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// Beside a `.part`: the validator of the version its bytes came from, which a
/// resume sends back as `If-Range`.
fn validator_of(part: &Path) -> PathBuf {
    let mut name = part.as_os_str().to_owned();
    name.push(".validator");
    PathBuf::from(name)
}

/// What a HEAD or a GET says about the file on the server.
#[derive(Debug, Clone, PartialEq)]
struct Remote {
    len: Option<u64>,
    /// A strong ETag, else Last-Modified: what `If-Range` accepts. A weak ETag
    /// is not one -- RFC 9110 refuses it there -- so it is passed over.
    validator: Option<String>,
}

impl Remote {
    fn of(headers: &ureq::http::HeaderMap) -> Remote {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        Remote {
            len: get("content-length").and_then(|v| v.parse().ok()),
            validator: validator(get("etag"), get("last-modified")),
        }
    }
}

fn validator(etag: Option<&str>, last_modified: Option<&str>) -> Option<String> {
    etag.filter(|e| e.starts_with('"'))
        .or(last_modified)
        .map(str::to_string)
}

/// Fetch one file, resuming a `.part` only while the server still has the
/// version its bytes came from.
///
/// Size alone cannot say that. Geofabrik republishes every extract daily, and
/// a `Range` request against a newer file appends the new file's tail to the
/// old one's head: the right length, no error, and a corrupt extract. So the
/// resume carries `If-Range` with the validator recorded when the bytes were
/// first written, and a server whose file has changed answers with the whole
/// new one instead, which is written over the fragment.
///
/// A finished file is a snapshot and is left alone: to fetch the current one,
/// delete it.
fn fetch(want: &Want, bar: bool, prefix: &str) -> Result<(), Error> {
    if let Ok(meta) = std::fs::metadata(&want.dest) {
        let remote = probe(&want.url).ok().and_then(|r| r.len);
        if remote.is_some_and(|r| r != meta.len()) {
            progress::warn(format!(
                "{}: kept the copy on disk, {} where the server now has {} \
                 -- delete it to fetch the current one",
                want.label,
                progress::bytes(meta.len()),
                progress::bytes(remote.unwrap_or(0)),
            ));
        }
        println!("    {prefix} have  {:>9}", progress::bytes(meta.len()));
        return Ok(());
    }

    let mut last: Option<Error> = None;
    for attempt in 1..=ATTEMPTS {
        match attempt_fetch(want, bar, prefix) {
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

fn attempt_fetch(want: &Want, bar: bool, prefix: &str) -> Result<(), Error> {
    let part = part_of(&want.dest);
    let stamp = validator_of(&part);
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let recorded = std::fs::read_to_string(&stamp)
        .ok()
        .filter(|v| !v.is_empty());
    let t0 = Instant::now();

    // A fragment whose version is known is complete already if the server
    // still has that version at that length -- a run killed between the last
    // byte and the rename. Asking for `bytes=<len>-` would be a 416.
    let remote = probe(&want.url)?;
    if have > 0 && recorded.is_some() && recorded == remote.validator && remote.len == Some(have) {
        return finish(&part, &stamp, &want.dest);
    }

    // Continue only from bytes of a known version, and only if it is still the
    // server's: `If-Range` turns the 206 into a 200 and the whole current file
    // when it is not. A fragment with no recorded version starts over.
    let mut request = ureq::get(&want.url);
    let resuming = have > 0 && recorded.is_some();
    if resuming {
        request = request
            .header("Range", &format!("bytes={have}-"))
            .header("If-Range", recorded.as_deref().unwrap_or_default());
    }
    let response = request.call()?;
    let resumed = resuming && response.status() == 206;
    if resumed {
        let range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok());
        if range_start(range) != Some(have) {
            return Err(format!("asked to resume at {have}, the server sent {range:?}").into());
        }
    }
    let got = Remote::of(response.headers());
    let total = if resumed {
        got.len.map(|n| n + have)
    } else {
        got.len
    };

    if let Some(parent) = want.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = if resumed {
        std::fs::OpenOptions::new().append(true).open(&part)?
    } else {
        // A new version: its validator is written before its first byte, so
        // no fragment on disk is ever without the version it belongs to.
        let _ = std::fs::remove_file(&stamp);
        let file = std::fs::File::create(&part)?;
        if let Some(v) = got.validator.as_ref().or(remote.validator.as_ref()) {
            std::fs::write(&stamp, v)?;
        }
        file
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
    finish(&part, &stamp, &want.dest)?;

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

/// The fragment is the file: give it its name, and drop its version stamp.
fn finish(part: &Path, stamp: &Path, dest: &Path) -> Result<(), Error> {
    std::fs::rename(part, dest)?;
    let _ = std::fs::remove_file(stamp);
    Ok(())
}

/// The first byte of a `Content-Range: bytes <first>-<last>/<len>`.
fn range_start(header: Option<&str>) -> Option<u64> {
    header?
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn probe(url: &str) -> Result<Remote, Error> {
    let response = ureq::head(url).call()?;
    Ok(Remote::of(response.headers()))
}

// --- Geofabrik's catalogue -------------------------------------------------

/// Geofabrik's own index of everything it publishes, cached in `pbf/`.
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
                progress::line("fetching Geofabrik's region index (once; cached in pbf/)");
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
        println!(
            "\nalso downloaded: {}",
            extra
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// One file behind HTTP/1.1, honouring `Range: bytes=N-` and `If-Range` the
    /// way Geofabrik does: a resume against a changed file gets the whole new
    /// one with a 200. `serve` swaps the file between requests.
    struct Server {
        url: String,
        file: Arc<Mutex<(Vec<u8>, String)>>,
        ranges: Arc<AtomicUsize>,
    }

    impl Server {
        fn start(body: &[u8], etag: &str) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/x.osm.pbf", listener.local_addr().unwrap());
            let file = Arc::new(Mutex::new((body.to_vec(), etag.to_string())));
            let ranges = Arc::new(AtomicUsize::new(0));
            let (f, r) = (file.clone(), ranges.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let head = line.starts_with("HEAD");
                    let (mut range, mut if_range) = (None, None);
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let (k, v) = h.split_once(':').unwrap();
                        match k.to_ascii_lowercase().as_str() {
                            "range" => range = v.trim().strip_prefix("bytes=").map(str::to_string),
                            "if-range" => if_range = Some(v.trim().to_string()),
                            _ => {}
                        }
                    }
                    let (body, etag) = f.lock().unwrap().clone();
                    let from = range
                        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                        .filter(|_| if_range.as_deref().is_none_or(|v| v == etag));
                    let mut out = stream;
                    let (status, part, extra) = match from {
                        Some(n) => {
                            r.fetch_add(1, Ordering::Relaxed);
                            let range = format!(
                                "Content-Range: bytes {n}-{}/{}\r\n",
                                body.len() - 1,
                                body.len()
                            );
                            ("206 Partial Content", &body[n..], range)
                        }
                        None => ("200 OK", &body[..], String::new()),
                    };
                    let _ = write!(
                        out,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: {etag}\r\n{extra}Connection: close\r\n\r\n",
                        part.len()
                    );
                    if !head {
                        let _ = out.write_all(part);
                    }
                }
            });
            Server { url, file, ranges }
        }

        fn serve(&self, body: &[u8], etag: &str) {
            *self.file.lock().unwrap() = (body.to_vec(), etag.to_string());
        }
    }

    fn want(dir: &Path, url: &str) -> Want {
        Want {
            label: "x".into(),
            url: url.into(),
            dest: dir.join("x.osm.pbf"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("minimap-dl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_fragment_of_the_same_version_resumes_and_takes_its_name() {
        let server = Server::start(b"0123456789", "\"v1\"");
        let dir = scratch("same");
        let w = want(&dir, &server.url);
        std::fs::write(part_of(&w.dest), b"0123").unwrap();
        std::fs::write(validator_of(&part_of(&w.dest)), "\"v1\"").unwrap();

        fetch(&w, false, "").unwrap();
        assert_eq!(std::fs::read(&w.dest).unwrap(), b"0123456789");
        assert_eq!(server.ranges.load(Ordering::Relaxed), 1, "it resumed");
        assert!(!part_of(&w.dest).exists() && !validator_of(&part_of(&w.dest)).exists());
    }

    /// The bug this module exists to not have: the server's file grew since the
    /// fragment was written, and a plain Range would have glued its tail on.
    #[test]
    fn a_fragment_of_a_changed_file_is_replaced_not_extended() {
        let server = Server::start(b"0123", "\"v1\"");
        let dir = scratch("changed");
        let w = want(&dir, &server.url);
        std::fs::write(part_of(&w.dest), b"01").unwrap();
        std::fs::write(validator_of(&part_of(&w.dest)), "\"v1\"").unwrap();
        server.serve(b"ABCDEFGHIJ", "\"v2\"");

        fetch(&w, false, "").unwrap();
        assert_eq!(std::fs::read(&w.dest).unwrap(), b"ABCDEFGHIJ");
        assert_eq!(
            server.ranges.load(Ordering::Relaxed),
            0,
            "nothing was appended"
        );
    }

    #[test]
    fn a_fragment_with_no_recorded_version_starts_over() {
        let server = Server::start(b"0123456789", "\"v1\"");
        let dir = scratch("unknown");
        let w = want(&dir, &server.url);
        std::fs::write(part_of(&w.dest), b"XX").unwrap();

        fetch(&w, false, "").unwrap();
        assert_eq!(std::fs::read(&w.dest).unwrap(), b"0123456789");
        assert_eq!(server.ranges.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_finished_file_is_kept_even_when_the_server_has_moved_on() {
        let server = Server::start(b"a newer, longer extract", "\"v2\"");
        let dir = scratch("kept");
        let w = want(&dir, &server.url);
        std::fs::write(&w.dest, b"the snapshot").unwrap();

        fetch(&w, false, "").unwrap();
        assert_eq!(std::fs::read(&w.dest).unwrap(), b"the snapshot");
    }

    #[test]
    fn a_fragment_complete_but_not_renamed_is_finished_without_a_request() {
        let server = Server::start(b"0123", "\"v1\"");
        let dir = scratch("done");
        let w = want(&dir, &server.url);
        std::fs::write(part_of(&w.dest), b"0123").unwrap();
        std::fs::write(validator_of(&part_of(&w.dest)), "\"v1\"").unwrap();

        fetch(&w, false, "").unwrap();
        assert_eq!(std::fs::read(&w.dest).unwrap(), b"0123");
    }

    #[test]
    fn if_range_takes_a_strong_etag_else_last_modified() {
        let lm = Some("Sat, 11 Oct 2026 01:02:03 GMT");
        assert_eq!(validator(Some("\"abc\""), lm).as_deref(), Some("\"abc\""));
        assert_eq!(validator(Some("W/\"abc\""), lm), lm.map(str::to_string));
        assert_eq!(validator(None, None), None);
        assert_eq!(range_start(Some("bytes 4-9/10")), Some(4));
        assert_eq!(range_start(Some("bytes */10")), None);
        assert_eq!(range_start(None), None);
    }
}
