# server

Serves the map: one PMTiles archive per layer over `/tiles/{layer}/{z}/{x}/{y}`,
the viewer that draws them, and — once `make anon` has run — the `/zone`
lookup. No database and no geometry code; a request is a binary search over an
mmapped file, and `make serve` runs it.

The rest of this README is about running the same thing *inside another
application*, because that is the question that actually comes up: you have an
axum service already, and you want the map under some path of it, or the zone
endpoint, or both.

## What an embedding is made of

Artifacts. All of them are build products of this repo — the host project
never needs DuckDB, the extracts, or anything else the pipeline wanted:

| | size (Europe) | produced by |
|---|---:|---|
| `pmtiles/<layer>.pmtiles` | 29 GB | `make all` |
| `anon/anon-zones.bin` | 70 MB | `make anon` |
| `server/web/` — `index.html`, `minimap.js` | 40 kB | in the repo |

Code, and here the two halves differ:

* **`anon-format` is a library** — the ~450 dependency-free lines under
  `anon/format/`. Depend on it directly.
* **This crate is a binary**, so its two reusable pieces are copied, not
  imported: `src/pmtiles.rs` (the archive reader, ~250 lines, wants `memmap2`
  and `flate2`) and the handlers in `src/main.rs` (`tile`, `meta_json`, and
  the `/zone` trio, ~250 lines between them).

Everything below assumes the artifacts sit somewhere on the host's disk and
get there by `scp` or the deploy pipeline — copying `pmtiles/` *is* the
deployment, as the top-level README's serving notes explain.

## Zones alone

```toml
[dependencies]
anon-format = { git = "https://github.com/julienfr112/minimap", rev = "<pin>" }
memmap2 = "0.9"
```

Pin the `rev`: the crate and the baked file have to agree byte for byte, and
they enforce it — `Index::parse` refuses a version it does not know rather
than misreading it, so a mismatched pair fails at startup instead of
answering nonsense.

```rust
struct Anon { map: memmap2::Mmap, index: anon_format::Index, tier: usize }

let file = std::fs::File::open("anon-zones.bin")?;
// SAFETY: the file is immutable for the life of the process. A re-bake
// writes a new file and the service restarts onto it.
let map = unsafe { memmap2::Mmap::map(&file)? };
let index = anon_format::Index::parse(&map)?;
let tier = index.tier(Some(64)).ok_or("no k=64 tier baked")?;
```

Put an `Arc` of that in your router's state; the lookup is one call,
`index.zone(&map, tier, lat, lon)`, plus `index.quads(&z)` if something wants
to draw the answer. `anon/serve/src/main.rs` is the whole reference
implementation in 200 lines, `zone()` in this crate's `main.rs` is the same
thing with the quads added. Resident cost is what the kernel pages in — a few
hundred bytes per query, whatever the index weighs.

## The map alone

Copy `src/pmtiles.rs`, open one `Archive` per `.pmtiles` file at startup (the
loop at the top of `main()` here), and copy the `tile` and `meta_json`
handlers. Two things in `tile` look incidental and are not:

* **`Content-Encoding: gzip`, pass-through.** Tiles are stored compressed and
  go to the client untouched. If the host app has a compression layer, exempt
  the tile route — re-encoding 30 GB of already-gzipped tiles buys latency and
  nothing else (the top-level README measures this).
* **The per-archive `ETag`.** Tiles never change within a build, so one etag
  per archive lets browsers skip re-downloading the map on every visit, and
  re-baking one layer does not invalidate the others.

The viewer is two static files with no build step; serve `web/` from any
static-file route.

## Both, nested

```rust
let map_state = Arc::new(MapApp { layers, anon, web });
let map = Router::new()
    .route("/meta.json", get(meta_json))
    .route("/tiles/{layer}/{z}/{x}/{y}", get(tile))
    .route("/zone", get(zone_from_query).post(zone_from_body))
    .fallback(get(asset))
    .with_state(map_state);

app = app.nest("/map", map);
```

`.with_state` on the nested router keeps its state separate from the host's,
so nothing about the existing extractors changes.

One caveat to nesting, currently: `minimap.js` fetches `/meta.json`,
`/tiles/…` and `/zone` by absolute path, so under `/map` those requests miss
the prefix. Until the viewer is made prefix-relative, either serve the three
routes at the root alongside your app, or patch the three fetch calls when
you copy `web/`.

## What the host application must not do

The code above is the easy half. These are the constraints a large codebase
breaks by accident, each the subject of a longer note in `anon/README.md`:

* **Log the `/zone` request line.** `lat`/`lon` are the thing being protected,
  and a `TraceLayer` on the outer router logs nested routes too. Exclude the
  route from URI logging, or expose POST only — the GET form exists for curl.
* **Turn `k` into a query parameter.** Tiers nest, so a caller who can choose
  keeps the smallest answer. Resolve the tier once at startup, from config.
* **Enrich the zone response.** Every field must stay a function of the zone
  alone — a distance-to-centre, a `contains` flag, or the input echoed back
  each leak bits the zone id does not contain. Build responses from `Zone`
  and `Extent` fields and nothing else.
* **Mutate the files under the mmaps.** A re-bake writes new files and the
  process restarts onto them; that contract is what makes the `unsafe` in
  `Mmap::map` sound.

## Operating cost

Everything is mmapped, so the host machine needs the disk and little else:
the kernel keeps hot pages resident and a Europe-sized 29 GB map serves fine
from a 1 GB VPS. See "Deploying on a small VPS" in the top-level README for
the measured numbers.
