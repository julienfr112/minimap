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

Code — and both halves are libraries, so nothing here is copied:

* **`anon-format`** — the ~450 dependency-free lines under `anon/format/`.
* **this crate** — `src/lib.rs` is the server (paths in, an axum `Router` out);
  `src/main.rs` is that library plus an environment and a listen address.
  `src/pmtiles.rs` is public too, for a host that wants the archive reader and
  none of the HTTP.

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

## Both, nested

```toml
minimap-server = { path = "…/minimap/server" }   # or a pinned git rev
```

```rust
let opts = minimap_server::Options {
    tiles: "/srv/pmtiles".into(),
    web: "…/minimap/server/web".into(),
    zones: Some("/srv/anon-zones.bin".into()),
    k: Some(64),
};
app = app.nest("/map", minimap_server::MapServer::open(&opts)?.router());
```

`.router()` applies its own state, so nothing about the host's extractors or
state type changes. The viewer's requests are all relative and the shell
redirects to a trailing slash first, so `/map/` works without the viewer
knowing it is nested.

**`open` fails when there are no archives**, deliberately: a map server with no
map is a misconfiguration. A host that wants to boot anyway — a test suite, a
dev machine without the 29 GB — should treat the error as "no map today" and
skip the `nest`, not paper over it inside the library.

## The viewer as a component

`web/minimap.js` is a page *and* a library. It publishes `window.Minimap`, and
its own bootstrap only runs when the standalone shell is there (it looks for
`#hud`), so a host application loads the same file, gets the class, and builds
its own maps:

```js
const meta = await (await fetch('/map/meta.json')).json();
const map = new Minimap(canvas, meta, { base: '/map/', interactive: false,
                                        keyboard: false, hash: false,
                                        query: false, anon: false });
map.setView(48.8584, 2.2945, 15);
map.boxes.push({ west, south, east, north, color: '#f00' });
map.pins.push({ lat, lon, image: img, onclick: () => select(i) });
map.dirty = true;
```

Every option defaults to what the standalone page does, and every one of them
names a way the viewer reaches **outside its canvas** — which is exactly what
is wrong in an embedding, and silently:

| | off means |
|---|---|
| `interactive` | no pan, no wheel, no dblclick — a picture, not a control |
| `keyboard` | no `window` keydown, so `-` typed in a form doesn't zoom |
| `hash` | the URL fragment is the host's; don't read a view out of it |
| `query` | `?maxzoom` is the host's query string, not a viewer flag |
| `anon` | a click is not a `POST /zone` |

`base` is the other kind of option — where the server is, not what the viewer
does. Empty (the default) means relative to the document, which is right for
the standalone shell because it *is* served by that server. A viewer embedded
in a host's own page is not: on `/calendar`, `tiles/…` resolves to
`/calendar/tiles/…`, and **the failure is silent** — an absent tile and a 404
are both "nothing to draw here", so the map comes out blank rather than broken.
Pass the prefix, with its trailing slash.

`boxes` and `pins` are plain arrays drawn on top of everything else; mutate
them and set `dirty = true`. A pin carries `{lat, lon, image, size?, anchor?,
tint?, onclick?}` — `image` is anything `drawImage` takes, and a pin whose
image has not decoded yet is skipped, so preload and mark dirty on `load`.
Clicks hit-test against where pins were actually drawn, and a pin takes the
click before the ground does.

Two things in the tile route look incidental and are not, if a host is
tempted to wrap it:

* **`Content-Encoding: gzip`, pass-through.** Tiles are stored compressed and
  go to the client untouched. If the host app has a compression layer, exempt
  the tile route — re-encoding 30 GB of already-gzipped tiles buys latency and
  nothing else (the top-level README measures this).
* **The per-archive `ETag`.** Tiles never change within a build, so one etag
  per archive lets browsers skip re-downloading the map on every visit, and
  re-baking one layer does not invalidate the others.

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

`make perf` measures the three things that stand between a request and its
bytes — the leaf-directory cache, the etag, and the page cache — and prints a
report rather than a verdict (`server/tests/cache_perf.rs`; it builds its own
archives, so it needs nothing from the pipeline). On this laptop, against a
synthetic 42 MB archive:

| | |
|---|---:|
| leaf-directory miss (gunzip + parse) | ~10 µs |
| leaf-directory hit | ~0.12 µs |
| tile request through the router, 32 in flight | ~0.9 µs each |
| the same request, one at a time | ~1.8 µs |
| what a 304 saves over it | the body, plus ~0.2 µs |
| RSS per request, steady state | 0 |

Two of those numbers matter to an embedding:

* **A hot tile does not scale across cores.** Every leaf hit takes the one
  `Mutex` in `Archive`, so 16 threads on the same tile get ~4.3 M lookups/s
  between them where one thread gets 19.6 M. It is 200 ns of lock, not a
  problem at any traffic this serves — but it is the ceiling, and it is where
  to look first if a host application ever finds one.
* **A cold miss on a real archive is a page fault, not a gunzip.** Against the
  15 GB `roads.pmtiles`, randomly-sampled tiles cost ~780 µs cold and ~1.8 µs
  warm, and the walk made ~700 kB resident per tile served — a tile's own
  bytes, the leaf it lives in, and the kernel's read-ahead around both. All of
  it evictable, which is the whole reason the archive may be larger than the
  machine. `make perf TILES=pmtiles` reproduces it, and pulls those GB through
  the page cache to do so.

Invalidation is asserted rather than measured, because it is a design
property: the etag is the archive's size and mtime read once at open, so a
re-baked layer invalidates that layer and no other, and a deploy that swaps
a file needs the process bounced — which is the same restart the mmap
contract above already requires.

## Sizing the host

Neither of those numbers sizes a machine on its own. What does is how much of
the archive stays resident, and that is a question about *traffic*, not about
the archive: people look at city centres, so the hot set is a small fraction of
what shipped. `make working-set` measures the fraction — for Europe, 2.5 GB of
a 27 GB archive covers 24 capitals and every shallow rung, which is why 8 GB is
comfortable and why the same box would struggle if the same requests were
spread evenly over the map.

An embedded viewer costs the host ~140 kB on first paint, ~30–50 kB per pan,
and nothing at all while it sits on screen — there is no session, no polling
and no socket to hold open, so idle users are genuinely free. "How many users a
small box holds" in the top-level README works this through to a number.
