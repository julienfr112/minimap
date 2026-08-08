//! What the server's caching costs, what invalidates it, and what it holds.
//!
//! Three things are cached between a request and the bytes it answers with, and
//! this measures each of them:
//!
//!   1. **Leaf directories** (`pmtiles::Archive::leaves`) -- the one place the
//!      server allocates per request-that-misses. A leaf is gunzipped and
//!      parsed on first touch and kept forever behind a `Mutex`, so the
//!      questions are how much the hit saves, how the lock behaves when every
//!      thread wants the same leaf, and how much memory the cache ends up
//!      holding when a client walks the whole archive.
//!   2. **Etags** -- one per archive, computed once at open from the file's
//!      size and mtime. That makes invalidation a deploy-and-restart, not a
//!      runtime event, and makes it per layer: re-baking `roads` must not cost
//!      a client its cached `land` tiles. Both are asserted below.
//!   3. **The kernel's page cache**, implicitly: nothing here reads the archive,
//!      it mmaps it, so "memory used" is resident pages the kernel is free to
//!      drop, plus a small bounded heap. The RSS phases separate the two.
//!
//! It runs against a synthetic archive built here (deterministic, a few tens of
//! MB, no `make all` required) and, when `MINIMAP_TILES` points at real
//! archives, against those as well -- which is the only way to see what a cold
//! page fault costs on a 15 GB file.
//!
//! ```text
//! make perf                                # the whole report
//! MINIMAP_PERF_SCALE=10 make perf          # ten times the iterations
//! MINIMAP_TILES=pmtiles make perf          # plus the real archives
//! ```
//!
//! Deliberately one `#[test]`: the phases share a process and two of them
//! measure RSS, which cargo's default parallelism would turn into noise.
//! Timings are printed rather than asserted -- a threshold that fails on a busy
//! laptop teaches nobody anything. What *is* asserted is the behaviour the
//! numbers are supposed to explain: warm beats cold, the cache stays bounded,
//! and invalidation happens exactly when the archive changes.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use http_body_util::BodyExt;
use minimap_server::{
    pmtiles::{tile_id, Archive},
    MapServer, Options,
};
use tower::ServiceExt;

/// Entries per leaf directory. Real archives pick this to keep the root under
/// the spec's 16 kB; here it is small on purpose, so a modest fixture still has
/// hundreds of leaves to miss on.
const LEAF_SIZE: usize = 128;

/// The fixture's deepest rung. z0..=8 is 87 381 tiles and ~680 leaves -- enough
/// that a walk over the archive cannot hold its directories in the root. A
/// debug build takes a shallower one: it is there to check the assertions, and
/// building 87 000 gzip members with the optimiser off costs more than the rest
/// of `cargo test` put together.
#[cfg(not(debug_assertions))]
const DEEP: u8 = 8;
#[cfg(debug_assertions)]
const DEEP: u8 = 6;

// --- the report -------------------------------------------------------------

#[test]
fn cache_perf() {
    let mut scale = std::env::var("MINIMAP_PERF_SCALE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0);
    // A debug build times a different program: bounds checks in the varint
    // parser, no inlining through the binary search. Run a twentieth of the
    // work so a plain `cargo test` stays a correctness run and says so, rather
    // than spending minutes producing numbers nobody should quote.
    if cfg!(debug_assertions) {
        scale *= 0.05;
        println!("\n*** debug build: timings below are not the server's -- use `make perf` ***");
    }

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cache-perf");
    fs::create_dir_all(&dir).unwrap();
    // Two layers, because half of what invalidation has to get right is that
    // the layers are independent.
    let roads = Fixture::write(&dir.join("roads.pmtiles"), DEEP, 0x9E37);
    let land = Fixture::write(&dir.join("land.pmtiles"), 5, 0x2545);

    println!("\nfixture  {}", dir.display());
    println!(
        "  roads  {:>7} tiles  {:>4} leaves  {:>7.1} MB",
        roads.tiles.len(),
        roads.leaves,
        roads.bytes as f64 / 1e6
    );
    println!(
        "  land   {:>7} tiles  {:>4} leaves  {:>7.1} MB",
        land.tiles.len(),
        land.leaves,
        land.bytes as f64 / 1e6
    );

    leaf_cache(&roads, scale);
    contention(&roads, scale);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(invalidation(&dir, &roads, &land, scale));
    rt.block_on(memory_under_load(&dir, &roads, scale));

    real_archives(scale);
    println!();
}

// --- 1. the leaf cache ------------------------------------------------------

/// Cold versus warm, and what the cold path leaves behind.
///
/// The miss is measured on its own: one tile per leaf against a freshly opened
/// archive, so every lookup pays a gunzip plus a varint parse of `LEAF_SIZE`
/// entries. Measuring it over a whole sweep instead would divide that cost by
/// the 127 hits that follow each miss and report a cache that barely matters.
/// A hit is two binary searches and a mutex round trip; the ratio between the
/// two is the whole reason the cache exists.
fn leaf_cache(fx: &Fixture, scale: f64) {
    section("1. leaf directory cache -- cold vs warm");

    // One tile out of each leaf, so this walk misses on every lookup.
    let first_of_each_leaf: Vec<_> = fx.tiles.iter().copied().step_by(LEAF_SIZE).collect();

    // A fresh archive holds only the root, parsed at open.
    let archive = Archive::open(&fx.path).unwrap();
    let before = rss();
    let (cold_bytes, cold) = timed(|| sum(&archive, &first_of_each_leaf));
    let after_cold = rss();
    let (warm_bytes, warm) = timed(|| sum(&archive, &first_of_each_leaf));

    // Same archive, same tiles: the cache must not have changed an answer.
    assert_eq!(
        cold_bytes, warm_bytes,
        "the warm pass answered differently from the cold one"
    );

    row("cold, every lookup a miss", first_of_each_leaf.len(), cold);
    row("warm, every lookup a hit", first_of_each_leaf.len(), warm);
    let speedup = cold.as_secs_f64() / warm.as_secs_f64();
    println!("  speedup                          {speedup:>8.1}x");

    // Only worth a cache if the hit is meaningfully cheaper. It is normally
    // 20-50x; 1.5 is the loose floor that says the mechanism is still there.
    assert!(
        speedup > 1.5,
        "warm lookups were only {speedup:.2}x cold -- the leaf cache is not caching"
    );

    // What a full walk costs once nothing misses any more -- the number a
    // client panning across the map actually experiences.
    let (_, sweep) = timed(|| sum(&archive, &fx.tiles));
    let after_sweep = rss();
    row("sweep of every tile, warm", fx.tiles.len(), sweep);

    // The cache holds one parsed directory per leaf and nothing per request, so
    // its size is a property of the archive. `Entry` is 24 bytes of payload in
    // a 32-byte slot, and each leaf's Vec is one allocation.
    println!(
        "  cache ceiling                    {:>8}  ({} leaves x {} entries)",
        bytes((fx.leaves * LEAF_SIZE * 32) as u64),
        fx.leaves,
        LEAF_SIZE
    );
    if let (Some(a), Some(b), Some(c)) = (before, after_cold, after_sweep) {
        // A floor, not a measurement: the allocator can satisfy these Vecs out
        // of memory it already holds, which is why the ceiling above is printed
        // next to it. Nothing here faults tile *data* in -- `tile()` hands back
        // a slice of the mmap without reading it, so only the router (phase 4)
        // makes the archive itself resident.
        println!(
            "  RSS filling {:>3} leaves           {:>8}  (resident growth, a floor)",
            fx.leaves,
            bytes(b.saturating_sub(a))
        );
        println!(
            "  RSS over the warm sweep          {:>8}  ({} lookups, nothing new to cache)",
            bytes(c.saturating_sub(b)),
            thousands(fx.tiles.len() as u64)
        );
        // The point of the phase: a client that keeps asking cannot keep
        // growing the server. The ceiling is structural, so anything beyond it
        // plus slack means the cache is keyed by something it should not be.
        let ceiling = (fx.leaves * LEAF_SIZE * 32) as u64;
        assert!(
            c.saturating_sub(a) < ceiling + (8 << 20),
            "{} of RSS for a cache that cannot exceed {} -- it is not bounded by \
             leaf count",
            bytes(c - a),
            bytes(ceiling)
        );
    }

    // Sustained hit rate, the steady state of a client panning around one area.
    let iters = scaled(2_000_000, scale);
    let mut rng = Rng::new(1);
    let hot: Vec<_> = (0..64).map(|_| fx.tiles[rng.below(fx.tiles.len())]).collect();
    let (_, d) = timed(|| {
        let mut acc = 0u64;
        for i in 0..iters {
            let (z, x, y) = hot[i % hot.len()];
            acc += archive.tile(z, x, y).map_or(0, |b| b.len() as u64);
        }
        acc
    });
    row("hot working set, single thread", iters, d);
}

/// Total bytes addressed, which is also what keeps the optimiser from deciding
/// the lookups had no effect.
fn sum(archive: &Archive, tiles: &[(u8, u32, u32)]) -> u64 {
    tiles
        .iter()
        .map(|&(z, x, y)| archive.tile(z, x, y).map_or(0, |b| b.len() as u64))
        .sum()
}

// --- 2. the lock ------------------------------------------------------------

/// What the `Mutex` around the leaf cache does to concurrency.
///
/// Every hit takes the lock, so this is the server's one shared-mutable point
/// on the tile path. Two access patterns bracket real traffic: a *spread* over
/// the whole archive (many leaves, short critical sections) and a *hot* single
/// tile, where every thread contends for the same entry -- which is exactly
/// what a popular view looks like.
fn contention(fx: &Fixture, scale: f64) {
    section("2. leaf cache under concurrency");

    let archive = Archive::open(&fx.path).unwrap();
    for &(z, x, y) in &fx.tiles {
        archive.tile(z, x, y); // warm: measure the lock, not the gunzip
    }
    let archive = &archive;

    let threads: Vec<usize> = {
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        [1, 2, 4, 8, 16].into_iter().filter(|&t| t <= cpus.max(1)).collect()
    };
    let per_thread = scaled(400_000, scale);

    for (name, spread) in [("spread over the archive", true), ("one hot tile", false)] {
        println!("  {name}");
        let mut baseline = 0.0;
        for &t in &threads {
            let start = Instant::now();
            // Each thread returns the bytes it addressed: the sum is what stops
            // the optimiser deciding the lookups had no effect, and a zero says
            // the threads were measuring an archive full of misses.
            let found: u64 = std::thread::scope(|s| {
                let workers: Vec<_> = (0..t)
                    .map(|tid| {
                        let tiles = &fx.tiles;
                        s.spawn(move || {
                            let mut rng = Rng::new(tid as u64 + 1);
                            let mut acc = 0u64;
                            for _ in 0..per_thread {
                                let (z, x, y) = if spread {
                                    tiles[rng.below(tiles.len())]
                                } else {
                                    tiles[tiles.len() / 2]
                                };
                                acc += archive.tile(z, x, y).map_or(0, |b| b.len() as u64);
                            }
                            acc
                        })
                    })
                    .collect();
                workers.into_iter().map(|w| w.join().unwrap()).sum()
            });
            let d = start.elapsed();
            let ops = per_thread * t;
            let rate = ops as f64 / d.as_secs_f64();
            if t == 1 {
                baseline = rate;
            }
            println!(
                "    {t:>2} threads  {:>12}/s  {:>8.1} ns/op  {:>5.2}x of one thread",
                thousands(rate as u64),
                d.as_secs_f64() * 1e9 / ops as f64,
                rate / baseline,
            );
            assert!(found > 0, "every lookup missed -- the threads measured nothing");
        }
    }
    println!(
        "  (the hot row is the Mutex: one lock per hit, so a tile everybody wants is\n   \
         served one thread at a time even though the data never changes again. The\n   \
         spread row is mostly the machine -- random access over the whole archive\n   \
         misses in L3 either way -- so read it as a floor, not as the lock's fault.)"
    );
}

// --- 3. invalidation --------------------------------------------------------

/// Etags: what they save, when they change, and what they must not touch.
///
/// The design is that an archive's etag is its size and mtime, read once at
/// open. So a client's cached tiles survive a restart of an unchanged deploy,
/// and a re-baked layer invalidates that layer alone.
async fn invalidation(dir: &Path, roads: &Fixture, land: &Fixture, scale: f64) {
    section("3. etag invalidation");

    let app = router(dir);
    let (z, x, y) = roads.tiles[roads.tiles.len() / 3];
    let path = format!("/tiles/roads/{z}/{x}/{y}");
    let (lz, lx, ly) = land.tiles[land.tiles.len() / 3];
    let land_path = format!("/tiles/land/{lz}/{lx}/{ly}");

    let (status, etag, body) = get(&app, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    let etag = etag.expect("a served tile carries an etag");
    assert!(!body.is_empty());

    let (land_status, land_etag, _) = get(&app, &land_path, None).await;
    assert_eq!(land_status, StatusCode::OK);
    let land_etag = land_etag.unwrap();
    assert_ne!(
        etag, land_etag,
        "two layers share an etag -- re-baking one would invalidate the other"
    );

    // The hit: a matching validator answers with no body at all.
    let (status, _, body) = get(&app, &path, Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "a matching etag must 304");
    assert!(body.is_empty(), "a 304 must not carry the tile");

    // Browsers send lists, and a stale entry alongside a fresh one still hits.
    let list = format!("\"deadbeef-0\", {etag}");
    let (status, _, _) = get(&app, &path, Some(&list)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "an etag list must be matched by element");

    // A validator from another layer must not satisfy this one.
    let (status, _, _) = get(&app, &path, Some(&land_etag)).await;
    assert_eq!(status, StatusCode::OK, "another layer's etag revalidated this one");

    let iters = scaled(20_000, scale);
    let (_, miss) = timed_async(|| async {
        for _ in 0..iters {
            let (s, _, _) = get(&app, &path, None).await;
            assert_eq!(s, StatusCode::OK);
        }
    })
    .await;
    let (_, hit) = timed_async(|| async {
        for _ in 0..iters {
            let (s, _, _) = get(&app, &path, Some(&etag)).await;
            assert_eq!(s, StatusCode::NOT_MODIFIED);
        }
    })
    .await;
    row("200 with the tile", iters, miss);
    row("304 revalidated", iters, hit);
    println!(
        "  saved per revalidation           {:>8.0} ns and {} of body",
        (miss.as_secs_f64() - hit.as_secs_f64()) * 1e9 / iters as f64,
        bytes(body_len(&app, &path).await as u64),
    );

    // Now re-bake `roads` -- a new file with a different length, which is how a
    // deploy arrives. The server is dropped first: its mmap is of the old file,
    // and the contract in `Archive::open` is that an archive never changes
    // under a running process.
    drop(app);
    let rebaked = Fixture::write(&dir.join("roads.pmtiles"), DEEP, 0xBEEF);
    assert_ne!(
        rebaked.bytes, roads.bytes,
        "the fixture rebuild must differ in size for the etag to move"
    );

    let app = router(dir);
    let (status, new_etag, _) = get(&app, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    let new_etag = new_etag.unwrap();
    assert_ne!(new_etag, etag, "a re-baked archive kept its etag -- clients would serve stale tiles");

    // The old validator is now worthless, and must not be honoured.
    let (status, _, body) = get(&app, &path, Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "a stale etag was revalidated");
    assert!(!body.is_empty());

    // ... while the untouched layer's clients keep their cache.
    let (status, again, _) = get(&app, &land_path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        again.as_deref(),
        Some(land_etag.as_str()),
        "re-baking roads changed land's etag -- invalidation is not per archive"
    );
    let (status, _, _) = get(&app, &land_path, Some(&land_etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);

    println!("  roads re-baked: etag {etag} -> {new_etag}, land unchanged at {land_etag}");
    println!(
        "  (invalidation is a restart: the etag is read at open, so a deploy that\n   \
         swaps the file needs the process bounced -- which is also what re-mmaps it)"
    );

    // Leave the fixture as the other phases expect to find it.
    Fixture::write(&dir.join("roads.pmtiles"), DEEP, 0x9E37);
}

// --- 4. memory under load ---------------------------------------------------

/// RSS across a sustained walk of the archive, through the real router.
///
/// Two phases, because they answer different questions. *Sweep* touches
/// everything: it faults in mmapped pages and fills the leaf cache, so RSS
/// climbs toward the size of the archive -- pages the kernel evicts under
/// pressure, which is what lets a 14 GB archive serve from a small VPS.
/// *Steady* re-requests a small working set: nothing new to cache, so any RSS
/// growth here would be a leak, and the per-request `Vec` of tile bytes is the
/// only allocation on the path.
async fn memory_under_load(dir: &Path, fx: &Fixture, scale: f64) {
    section("4. memory under load (through the router)");

    let app = Arc::new(router(dir));
    let concurrency = 32;
    let sweep = scaled(fx.tiles.len(), scale).min(fx.tiles.len() * 4);
    let steady = scaled(200_000, scale);

    // The tile list is shared with the tasks, which outlive this frame as far
    // as the spawner is concerned.
    let tiles = Arc::new(fx.tiles.clone());

    let base = rss();
    let all = tiles.clone();
    let (_, d) = timed_async(|| async {
        hammer(
            &app,
            concurrency,
            sweep,
            Arc::new(move |i| {
                let (z, x, y) = all[i % all.len()];
                format!("/tiles/roads/{z}/{x}/{y}")
            }),
        )
        .await
    })
    .await;
    let after_sweep = rss();
    row("sweep, cold-ish, 32 in flight", sweep, d);

    let hot = tiles.clone();
    let (_, d) = timed_async(|| async {
        hammer(
            &app,
            concurrency,
            steady,
            Arc::new(move |i| {
                let (z, x, y) = hot[(i * 37) % 256];
                format!("/tiles/roads/{z}/{x}/{y}")
            }),
        )
        .await
    })
    .await;
    let after_steady = rss();
    row("steady, 256-tile working set", steady, d);

    if let (Some(a), Some(b), Some(c)) = (base, after_sweep, after_steady) {
        let swept = b.saturating_sub(a);
        let held = c.saturating_sub(b);
        println!(
            "  RSS over the sweep               {:>8}  (archive is {}; mmapped, so \
             evictable)",
            bytes(swept),
            bytes(fx.bytes)
        );
        println!(
            "  RSS over {} steady requests  {:>8}  ({:.1} B per request)",
            thousands(steady as u64),
            bytes(held),
            held as f64 / steady as f64
        );
        // The steady phase allocates one Vec per request and frees it. Anything
        // that scales with request count is a leak; 32 MB is slack for the
        // allocator's own arenas, not a budget.
        assert!(
            held < 32 << 20,
            "{} of RSS growth over {steady} requests to a 256-tile working set \
             -- something on the tile path is retaining",
            bytes(held)
        );
    }
}

// --- 5. the real archives ---------------------------------------------------

/// The same two measurements against whatever `MINIMAP_TILES` points at.
///
/// Only here does the interesting number appear: on a multi-GB archive most
/// leaves and most tiles are *not* resident, so a miss costs a page fault
/// against the disk rather than a gunzip against warm memory -- which is the
/// case the cache was built for and the fixture cannot reproduce.
fn real_archives(scale: f64) {
    let Ok(dir) = std::env::var("MINIMAP_TILES") else {
        section("5. real archives -- skipped");
        println!("  `make perf TILES=pmtiles` measures the archives that shipped as well,");
        println!("  which is where a miss stops being a gunzip and becomes a page fault");
        return;
    };
    section("5. real archives");

    // Cargo runs a test binary from the *package* directory, so a relative
    // `MINIMAP_TILES=pmtiles` -- the spelling the Makefile and the README use,
    // and the one anyone will try -- points at server/pmtiles. Fall back to the
    // repo root, which is what it meant.
    let dir = PathBuf::from(&dir);
    let dir = if dir.is_dir() {
        dir
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(&dir)
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            println!("  {}: {e}", dir.display());
            return;
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(layer) = name.strip_suffix(".pmtiles") {
            found.push((layer.to_string(), entry.path(), entry.metadata().unwrap().len()));
        }
    }
    found.sort_by_key(|(_, _, size)| std::cmp::Reverse(*size));
    let Some((layer, path, size)) = found.first() else {
        println!("  no .pmtiles in {}", dir.display());
        return;
    };
    println!("  {layer}  {:.1} GB", *size as f64 / 1e9);

    let archive = Archive::open(path).unwrap();
    // Tiles have to be discovered: the reader answers z/x/y, it does not
    // enumerate. Sample the archive's own bounding box, deepest rung first --
    // a bounding box is mostly sea and unbuilt land, so a rung where the sample
    // comes up nearly empty is the wrong rung to measure and the next one up
    // is tried instead.
    let want = scaled(2_000, scale).min(50_000);
    let mut tiles = Vec::with_capacity(want);
    let mut attempts = 0usize;
    for z in (archive.min_zoom..=archive.max_zoom).rev() {
        let (x0, y0) = to_tile(archive.min_lon, archive.max_lat, z);
        let (x1, y1) = to_tile(archive.max_lon, archive.min_lat, z);
        let mut rng = Rng::new(7);
        tiles.clear();
        for _ in 0..want * 4 {
            attempts += 1;
            let x = x0 + rng.below((x1 - x0 + 1) as usize) as u32;
            let y = y0 + rng.below((y1 - y0 + 1) as usize) as u32;
            if archive.tile(z, x, y).is_some() {
                tiles.push((z, x, y));
                if tiles.len() >= want {
                    break;
                }
            }
        }
        if tiles.len() >= want / 4 {
            break;
        }
    }
    if tiles.is_empty() {
        println!("  found no tiles by sampling the bounds -- nothing to measure");
        return;
    }
    println!(
        "  sampled {} tiles at z{} in {} probes",
        tiles.len(),
        tiles[0].0,
        thousands(attempts as u64)
    );

    // Fresh archive: leaf cache empty, page cache whatever the OS kept. That
    // second part is not controllable from here, so the cold number is a floor
    // on a truly cold server, not an estimate of one.
    let cold_archive = Archive::open(path).unwrap();
    // Copying the bytes out, which is what the handler does and what actually
    // faults the tile's pages in -- a lookup on its own only computes a slice
    // of the mmap and never reads it.
    let copy = |a: &Archive| -> u64 {
        tiles
            .iter()
            .map(|&(z, x, y)| a.tile(z, x, y).map(<[u8]>::to_vec).map_or(0, |b| b.len() as u64))
            .sum()
    };
    let before = rss();
    let (_, cold) = timed(|| copy(&cold_archive));
    let after = rss();
    let (bytes_read, warm) = timed(|| copy(&cold_archive));
    row("cold, tiles copied out", tiles.len(), cold);
    row("warm, tiles copied out", tiles.len(), warm);
    println!(
        "  mean tile                        {:>8}",
        bytes(bytes_read / tiles.len() as u64)
    );
    if let (Some(a), Some(b)) = (before, after) {
        let grew = b.saturating_sub(a);
        println!(
            "  RSS over the cold pass           {:>8}  ({} per tile served)",
            bytes(grew),
            bytes(grew / tiles.len() as u64)
        );
        println!(
            "  (a tile's own bytes are {} of that -- the rest is the leaf it lives in\n   \
             and the kernel's read-ahead around it, all of it evictable. This is why\n   \
             the archive can be larger than the machine.)",
            bytes(bytes_read / tiles.len() as u64)
        );
    }
}

/// Web-Mercator lon/lat to the tile containing it at zoom `z`.
fn to_tile(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0);
    let rad = lat.to_radians();
    let y = ((1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .clamp(0.0, n - 1.0);
    (x as u32, y as u32)
}

// --- the router harness -----------------------------------------------------

fn router(tiles: &Path) -> axum::Router {
    MapServer::open(&Options {
        tiles: tiles.to_path_buf(),
        web: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web"),
        // The zone index is a different mmap and a different question; nothing
        // on the tile path touches it.
        zones: None,
        k: None,
    })
    .unwrap()
    .router()
}

/// One request through the router: status, etag, body.
async fn get(
    app: &axum::Router,
    path: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, Option<String>, Vec<u8>) {
    let mut req = Request::builder().uri(path);
    if let Some(v) = if_none_match {
        req = req.header(header::IF_NONE_MATCH, v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let etag = res
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, etag, body)
}

async fn body_len(app: &axum::Router, path: &str) -> usize {
    get(app, path, None).await.2.len()
}

/// `total` requests with `concurrency` of them in flight, which is what a
/// browser opening a map view looks like.
async fn hammer(
    app: &Arc<axum::Router>,
    concurrency: usize,
    total: usize,
    url: Arc<dyn Fn(usize) -> String + Send + Sync>,
) {
    let next = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let app = app.clone();
        let next = next.clone();
        let url = url.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                if i >= total {
                    return;
                }
                let (status, _, _) = get(&app, &url(i), None).await;
                assert!(
                    status == StatusCode::OK || status == StatusCode::NO_CONTENT,
                    "unexpected {status} under load"
                );
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

// --- a synthetic PMTiles archive --------------------------------------------

/// A valid PMTiles v3 archive, written here rather than baked.
///
/// The perf question is about directories and lookups, not about map data, so
/// the tiles hold deterministic filler. What has to be real is the structure:
/// gzipped root and leaves, one leaf level, entries in Hilbert order -- because
/// that structure is what the cache is caching.
struct Fixture {
    path: PathBuf,
    /// Every z/x/y in the archive, in id order. The perf phases sample this
    /// rather than guessing coordinates.
    tiles: Vec<(u8, u32, u32)>,
    leaves: usize,
    bytes: u64,
}

impl Fixture {
    fn write(path: &Path, max_zoom: u8, salt: u64) -> Fixture {
        // Every position of every rung, sorted the way the format wants them.
        let mut all: Vec<(u64, u8, u32, u32)> = Vec::new();
        for z in 0..=max_zoom {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    all.push((tile_id(z, x, y).unwrap(), z, x, y));
                }
            }
        }
        all.sort_by_key(|&(id, ..)| id);

        // Tile bodies, gzipped as the format requires and as the server assumes
        // when it passes them through with Content-Encoding.
        let mut data = Vec::new();
        let mut entries = Vec::with_capacity(all.len());
        for &(id, z, ..) in &all {
            let body = filler(id ^ salt, z);
            let gz = gzip(&body);
            entries.push(Entry {
                tile_id: id,
                offset: data.len() as u64,
                length: gz.len() as u32,
                run_length: 1,
            });
            data.extend_from_slice(&gz);
        }

        // One leaf per LEAF_SIZE entries; the root points at the leaves, which
        // is the shape that makes a lookup miss twice before it can hit.
        let mut leaf_section = Vec::new();
        let mut root = Vec::new();
        for chunk in entries.chunks(LEAF_SIZE) {
            let gz = gzip(&serialise(chunk));
            root.push(Entry {
                tile_id: chunk[0].tile_id,
                offset: leaf_section.len() as u64,
                length: gz.len() as u32,
                // 0 is what marks a root entry as a pointer into the leaf
                // section rather than at a tile.
                run_length: 0,
            });
            leaf_section.extend_from_slice(&gz);
        }
        let leaves = root.len();
        let root_gz = gzip(&serialise(&root));

        let meta = format!(
            r#"{{"name":"perf","rungs":[{}],"attribution":"synthetic"}}"#,
            (0..=max_zoom).map(|z| z.to_string()).collect::<Vec<_>>().join(",")
        );
        let meta_gz = gzip(meta.as_bytes());

        // header | root | metadata | leaves | tiles
        let root_off = 127u64;
        let meta_off = root_off + root_gz.len() as u64;
        let leaf_off = meta_off + meta_gz.len() as u64;
        let data_off = leaf_off + leaf_section.len() as u64;

        let mut h = vec![0u8; 127];
        h[0..7].copy_from_slice(b"PMTiles");
        h[7] = 3;
        put_u64(&mut h, 8, root_off);
        put_u64(&mut h, 16, root_gz.len() as u64);
        put_u64(&mut h, 24, meta_off);
        put_u64(&mut h, 32, meta_gz.len() as u64);
        put_u64(&mut h, 40, leaf_off);
        put_u64(&mut h, 48, leaf_section.len() as u64);
        put_u64(&mut h, 56, data_off);
        put_u64(&mut h, 64, data.len() as u64);
        put_u64(&mut h, 72, all.len() as u64); // addressed tiles
        put_u64(&mut h, 80, all.len() as u64); // tile entries
        put_u64(&mut h, 88, all.len() as u64); // distinct contents
        h[96] = 1; // clustered
        h[97] = 2; // internal compression: gzip -- the reader insists
        h[98] = 2; // tile compression: gzip
        h[99] = 1; // tile type: MVT
        h[100] = 0;
        h[101] = max_zoom;
        put_i32(&mut h, 102, -1800000000); // the whole world, in 1e7 degrees
        put_i32(&mut h, 106, -850511287);
        put_i32(&mut h, 110, 1800000000);
        put_i32(&mut h, 114, 850511287);
        h[118] = max_zoom / 2;
        put_i32(&mut h, 119, 0);
        put_i32(&mut h, 123, 0);

        let mut out = Vec::with_capacity(data_off as usize + data.len());
        out.extend_from_slice(&h);
        out.extend_from_slice(&root_gz);
        out.extend_from_slice(&meta_gz);
        out.extend_from_slice(&leaf_section);
        out.extend_from_slice(&data);
        // Written whole and renamed, so a reader never sees a half-written
        // archive -- and so the mtime the etag is built from is the moment the
        // file became complete.
        let tmp = path.with_extension("pmtiles.tmp");
        fs::write(&tmp, &out).unwrap();
        fs::rename(&tmp, path).unwrap();

        // Reading it back through the reader under test is the cheapest
        // possible guard against measuring a fixture that is subtly wrong.
        let check = Archive::open(path).unwrap();
        let (_, z, x, y) = all[all.len() / 2];
        assert!(
            check.tile(z, x, y).is_some(),
            "the fixture is malformed: {z}/{x}/{y} is missing"
        );

        Fixture {
            path: path.to_path_buf(),
            tiles: all.into_iter().map(|(_, z, x, y)| (z, x, y)).collect(),
            leaves,
            bytes: out.len() as u64,
        }
    }
}

#[derive(Clone, Copy)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

/// A directory: count, then four columnar runs of varints. Offsets are always
/// written explicitly (`offset + 1`) rather than using the contiguous-run
/// shorthand -- the shorthand saves bytes in a real archive and changes nothing
/// about what a lookup costs.
fn serialise(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, entries.len() as u64);
    let mut last = 0u64;
    for e in entries {
        varint(&mut out, e.tile_id - last);
        last = e.tile_id;
    }
    for e in entries {
        varint(&mut out, u64::from(e.run_length));
    }
    for e in entries {
        varint(&mut out, u64::from(e.length));
    }
    for e in entries {
        varint(&mut out, e.offset + 1);
    }
    out
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i32(b: &mut [u8], at: usize, v: i32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// Stand-in tile bytes: a few hundred of them, deterministic, and compressible
/// like a vector tile rather than like noise.
fn filler(seed: u64, z: u8) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let len = 200 + (z as usize * 60) + rng.below(400);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes()[..4]);
        out.extend_from_slice(b"minimap");
    }
    out.truncate(len);
    out
}

// --- measuring --------------------------------------------------------------

/// Resident set size, in bytes. Linux only: `/proc/self/statm`'s second field
/// is resident pages. Elsewhere the memory phases print nothing rather than
/// guessing.
fn rss() -> Option<u64> {
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

async fn timed_async<F, Fut, T>(f: F) -> (T, Duration)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let start = Instant::now();
    let out = f().await;
    (out, start.elapsed())
}

fn scaled(n: usize, scale: f64) -> usize {
    ((n as f64 * scale) as usize).max(1)
}

fn section(title: &str) {
    println!("\n{title}\n{}", "-".repeat(title.len()));
}

fn row(label: &str, ops: usize, d: Duration) {
    println!(
        "  {label:<32} {:>12}/s  {:>8.0} ns/op",
        thousands((ops as f64 / d.as_secs_f64()) as u64),
        d.as_secs_f64() * 1e9 / ops as f64
    );
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} kB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// xorshift64*, so every run samples the same tiles in the same order. A perf
/// number that moves because the sample moved is not a perf number.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}
