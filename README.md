# minimap

A small, working vector-tile map built from raw OpenStreetMap data. It defaults
to **Picardie** (one 133 MB extract) for fast iteration; `--regions` scales it up.

The point of the design is that *all* the geometric work happens once, offline,
in DuckDB SQL. What is left to serve at runtime is a keyed blob lookup, so the
backend has no geometry code, no protobuf library and no database client.

```
 .osm.pbf  --pipeline-->  DuckDB `features`  --SQL-->  DuckDB `tiles`  -->  HTTP  -->  canvas
  133 MB     16 cores     1.03M features              12,809 MVT blobs     ~180 lines  pure JS
                          EPSG:3857                   78.5 MB              no database  no deps
```

Two binaries, and the split between them is the whole idea: `minimap` builds the
archive and pulls in DuckDB, GEOS and a PBF parser; `minimap-backend` serves it
and pulls in none of them. They come out 50 MB and 1.9 MB.

Measured on Picardie at z6–14: **load 16 s, bake 35 s, export 3 s** — 53 s from a
downloaded extract to a servable archive.

## Quick start

```bash
cargo run --release --bin minimap -- all      # download, load, bake, export
cargo run --release --bin minimap-backend     # http://127.0.0.1:8090
```

The first build takes a few minutes, because `duckdb`'s `bundled` feature
compiles DuckDB itself — the payoff is that there is no system library to
install and no way for the spatial extension to be a version out of step.

Pick a different extract with `--regions` (see `SOURCES` in
`pipeline/src/config.rs`):

```bash
cargo run --release --bin minimap -- all --regions picardie nord-pas-de-calais
```

`--regions` takes everything after it, so it goes last.

To work at country granularity, fetch the European extracts once:

```bash
./fetch-europe.sh            # 49 extracts, 31.7 GB, into data/countries/
JOBS=6 ./fetch-europe.sh     # faster, less polite to a free service
cargo run --release --bin minimap -- load --regions belgium netherlands luxembourg
```

It is resumable and re-runnable: finished files are skipped by comparing local
size to the server's `content-length`, partial ones continue via `curl -C -`.
`--regions` auto-discovers whatever has been downloaded. Four Geofabrik
aggregates are deliberately excluded because they overlap their own siblings and
would be counted twice: `alps`, `dach`, `britain-and-ireland`, and
`united-kingdom` (which overlaps `great-britain` plus Northern Ireland).

The viewer takes a `#zoom/lat/lon` hash, so a view can be linked directly — for
the Saint-Leu quarter of Amiens:

    http://127.0.0.1:8090/#16/49.89870/2.30160

The backend exposes three routes, and `web/` needs nothing else:

| route | returns |
| --- | --- |
| `GET /` | the viewer |
| `GET /meta.json` | TileJSON: zoom range, bounds, centre |
| `GET /tiles/{z}/{x}/{y}` | one MVT tile, or `204` if empty |

## How it works

**`minimap download`** fetches the Geofabrik extracts named by `--regions`.

**`minimap load`** parses the PBFs and appends the WKB straight into a `raw`
staging table. One SQL pass then classifies every object into a layer
(`roads` / `water` / `landuse` / `buildings`) and a class (`motorway`,
`farmland`, …), reprojects it to EPSG:3857, and caches its bounding box.

The division of labour is deliberate: the parser decides only *whether* an
object is drawable, and everything about *what it is* is decided in SQL, in one
pass over a columnar table. Classifying per object during extraction would put a
branch per row in the one place that is already the bottleneck.

Each feature also gets a **`minzoom`** — the shallowest zoom at which it is
worth drawing. For roads this comes from the class (motorways from z6,
residential streets from z11). For areas it is computed from their size:

```
minzoom = ceil(log2(MIN_PIXELS * MPP0 / sqrt(area)))
```

This is the single most effective data-reduction lever in the pipeline, and it
is why the whole region fits in a handful of megabytes.

**`minimap bake`** turns features into tiles, entirely in SQL. For each zoom, a
bbox-arithmetic join explodes every feature across the tiles it touches (no
spatial index needed), `ST_AsMVTGeom` clips and quantises it onto the 4096-unit
tile grid, and `ST_AsMVT` encodes one blob per layer. The per-layer blobs are
then concatenated — a `Tile` protobuf is just `repeated Layer layers = 3`, so
concatenation yields a valid multi-layer tile.

**The backend** does one `SELECT data FROM tiles WHERE z=? AND x=? AND y=?`.
That is the whole runtime.

Each tile carries exactly one attribute per feature, `cls`, because that is all
the styling needs. Street names stay in the `features` table — the viewer draws
no labels, and carrying names would inflate every tile by roughly 40%.

**The frontend** (`web/minimap.js`) has no dependencies. It contains a ~50-line
protobuf reader, an ~80-line MVT decoder, and a canvas renderer. Because tile
coordinates arrive as integers on a 0..4096 grid, drawing is a single affine map
to screen pixels.

## Layout

```
pipeline/          the build tool: download / load / bake / export / info
  config.rs          every constant and SQL fragment the pipeline is tuned by
  extract.rs         .osm.pbf -> staging rows, on every core
  geom.rs            ring assembly, hole nesting, WKB
  load.rs            raw -> features (classification SQL)
  bake.rs            features -> tiles (tiling SQL)
  export.rs          tiles -> minimap.pmtiles
backend/           serves the archive: axum + mmapped PMTiles, no database
web/index.html     page shell
web/minimap.js     protobuf reader + MVT decoder + canvas renderer
fetch-europe.sh    bulk-fetch the 49 European country extracts
minimap.duckdb     generated: features + tiles + meta (stays on the build machine)
minimap.pmtiles    generated: the archive the backend serves
```

## Notes and limitations

**Zoom range is z6–z14** (`MINZOOM` / `MAXZOOM` in `pipeline/src/config.rs`).
Past z14 the viewer overzooms, reusing z14 tiles scaled up.

**`MAXZOOM` is the main cost dial**, because the size filter is derived from it.
At z12 a tile spans ~6.3 km at latitude 50°, so a 10 m building is ~0.8 px and
the filter discards essentially all of the ~1.8M buildings in the extract —
correct, but it means no buildings. z14 brings the threshold down to ~18 m, so
buildings appear and a city quarter is legible; it also multiplies the tile count
by ~16 and is what makes the database interesting in size rather than trivial.
Changing `MAXZOOM` invalidates the staged extracts, so re-run `load`, not just
`bake`.

**Why not GDAL.** DuckDB's spatial extension can read `.osm.pbf` directly via
GDAL (`ST_Read`), and it is tempting because it needs no extra dependency. It
does not work here: GDAL's OSM driver requires *interleaved reading* across its
layers to emit everything, and DuckDB reads one layer at a time. Measured
against Picardie, `ST_Read(layer='multipolygons')` returned **3,780** polygons
where the file actually contains **1,834,429** building ways and **71,756**
landuse ways — a silent >99.9% loss. Roads happened to come through intact, but
a parser that drops most of its input without erroring is not one to build on.

**Why the PBF parser is written out.** The pipeline began as Python driving
pyosmium, and that loop was the whole cost of it: ~240 s of a 253 s `load`,
single threaded, because every OSM object crossed into Python to be looked at.
`extract.rs` does the same job in **7.2–7.8 s** — roughly half of that from using
16 cores instead of one, half from an object never becoming an object.

The interesting part is what had to be reimplemented. libosmium hands you
assembled areas; a raw PBF hands you nothing, because it stores nodes, then
ways, then relations, so nothing can be resolved on the pass that reads it. So
`extract.rs` makes three passes — relations, then ways, then nodes — which sounds
worse and is not: decompression is the cost of a pass and it parallelises, and
knowing *which* nodes matter before storing any is what avoids libosmium's
`flex_mem`, the thing that was going to stop this reaching Europe. Assembling
multipolygon rings and deciding which is a hole in which is then about 200 lines
(`pipeline/src/geom.rs`), because OSM `outer`/`inner` roles cannot be trusted and
nesting has to be settled by containment.

**How it was checked.** Before pyosmium was removed, both parsers were run over
Picardie into separate databases and the two `features` tables compared. They
agreed exactly:

| | this | pyosmium |
| --- | --- | --- |
| objects staged | 1,509,510 | 1,509,510 |
| features | 1,034,488 | 1,034,488 |
| per layer / class / minzoom | identical | identical |
| `(layer, osm_id)` present in one only | 0 | 0 |
| `ST_Area` / `ST_Length` differing by >1e-6 relative | 0 | 0 |
| parse wall clock | **7.2 s** | 262 s |

Baking both gave the same 12,809 tiles with the same `(z, x, y)` set and total
tile bytes within 0.003%. Only 938 of the tiles were byte-identical, which is
not a discrepancy: `ORDER BY cell` leaves ties unordered, so the two runs lay
features out in a different physical order inside a cell, `ST_AsMVT` encodes
them in that order, and delta encoding produces different bytes for the same
shapes. The same comparison was repeated against the port to a single Rust
binary, which reproduces all 1,034,488 features and all 12,809 tiles unchanged.

Getting there took two fixes, both of them cases where the obvious
implementation is quietly wrong:

- **Segment cancellation.** Assembling a multipolygon from whole member ways
  shatters any relation whose members share a wall — nine adjacent building
  outlines stay nine polygons instead of becoming one. 25 of Picardie's 5,160
  multipolygons came out wrong. The fix is to explode members into segments and
  drop the ones that appear twice, which is what libosmium does and what the
  multipolygon algorithm actually is.
- **`area=no`.** A closed way with `leisure=pitch, area=no` is an athletics
  track: a loop, not a surface. Ignoring the tag fills it in.

And one deliberate copy of libosmium: ring segments are keyed on node
*positions*, not node ids. Keying on ids looks more principled, but two distinct
nodes at the same coordinate are a common enough OSM error that Picardie has a
relation which only closes if you compare positions.

**Extracts overlap** along shared borders, so the loader de-duplicates on OSM id
when you pass more than one region.

**The database is much bigger than the tiles it serves** — ~1200 MB on disk for
78.5 MB of tiles. `features` accounts for 371 MB (1.03M full-precision geometries,
kept so re-baking never re-parses the PBF), `tiles` for 87 MB; the rest is space
DuckDB does not return after the `raw` staging table is dropped. At Europe scale
that dead space would be hundreds of GB, so the staging table should go —
classification could run per batch straight into `features`. Not done yet.

Only `tiles` and `meta` are read at serve time, so the build database never needs
to reach the server.

**Bulk loading must not go through `INSERT`.** An earlier version inserted the
staged rows one statement at a time and spent over 40 minutes without finishing a
single region — DuckDB is columnar, so each row paid the whole statement pipeline
plus a WAL write. The appender is a different thing entirely: it writes values
into a DuckDB vector and hands over a whole column chunk at a time, so
`append_row` names a row without costing one.

**Link jemalloc.** This is worth ~2× on the bake and is not a micro-optimisation.
DuckDB's own Linux builds link jemalloc; the amalgamation that `duckdb`'s
`bundled` feature compiles does not, and the bake is one long storm of small GEOS
allocations across 16 threads, which is what glibc malloc is worst at. Measured
on Picardie: **79.9 s of bake without it, 35 s with**. The feature that matters
is `unprefixed_malloc_on_supported_platforms` — it makes jemalloc export plain
`malloc`/`free` so DuckDB's C++ is redirected too. A `#[global_allocator]` on its
own only covers Rust's allocations, which are a rounding error here.

## Serving

All numbers below are measured on the real 12,809-tile Picardie set, not estimated.

### Do not serve from DuckDB

DuckDB is the right tool for the bake and the wrong one for the lookup. It is
columnar, so fetching one row still touches a row group and decompresses column
segments, and per-query pipeline setup costs milliseconds:

| store | per lookup | throughput | on disk (78.5 MB of blobs) |
| --- | --- | --- | --- |
| DuckDB | 4,072 µs | 246/s | — |
| SQLite (MBTiles shape) | 29.2 µs | 34,224/s | 94 MB (+20%) |
| LMDB, zero-copy | 0.41 µs | 2,410,708/s | 95 MB (+22%) |
| flat blob + index | 0.40 µs | 2,518,565/s | 75 MB (+0%) |

Measured end-to-end over HTTP with DuckDB behind it: **median 3.9 ms/tile, 248
req/s**, and the backend holds a single `Mutex<Connection>` so concurrent
requests queue. A viewport pulls ~20 tiles, so that is ~80 ms of serialised
database time. LMDB and the flat file both hand back a `memoryview` into the
mmap, so bytes reach the socket without being copied; SQLite copies each blob.

### Compression: store it pre-compressed

| | size | ratio |
| --- | --- | --- |
| gzip -9 | 67% | 1.48× |
| zstd -19 | 66% | 1.51× |
| brotli q11 | 63% | 1.58× |

Only ~35%, because MVT is already zigzag-varint delta-encoded. Compress **at bake
time** and serve with `Content-Encoding: gzip`: storage and egress both drop by a
third at zero per-request CPU. Never compress per request. gzip is the practical
choice; brotli buys 4% more for worse tooling support.

### Two optimisations that measurably do not work

- **Content-addressed dedup.** 12,786 of 12,809 tiles are unique (99.8%), saving
  **0.0%**. Empty tiles are already omitted as `204`s, which is where the
  duplicates would have been.
- **Shrinking the MVT extent.** 4096 → 512, an 8× coarser grid, saves **6.3%**.
  Delta encoding already makes neighbouring vertices cost 1–2 bytes regardless of
  the absolute grid. Not worth the precision loss.

### Deploying on a small VPS

This is the constraint that matters, and it inverts some of the advice above. A
cheap VPS has ~1–4 GB RAM, 20–80 GB of frequently network-attached disk, 1–2
vCPU, and often metered egress — not the 36 GB and local NVMe these numbers were
taken on.

**mmap is not RAM-resident.** Both the flat file and LMDB map the file and let
the kernel page in the 4 kB blocks actually touched, evicting under pressure. A
23 GB tile archive works on a 1 GB box. Neither approach loads the file.

**But the sub-microsecond figures assume a warm page cache.** With a small cache,
most lookups reach the disk. Measured on local NVMe with the cache explicitly
evicted via `posix_fadvise(DONTNEED)`:

| access pattern | median | p95 |
| --- | --- | --- |
| cold random 8 kB read | 112 µs | 159 µs |
| warm (same blocks) | 2.4 µs | 4.5 µs |
| cold but **adjacent** blocks | 1.2 µs | 77 µs |

So expect ~100 µs per tile cold on local NVMe, and worse on network-attached VPS
storage. Two consequences:

1. **Physical layout matters more than the store.** That last row is why tiles
   should be written in `cell` (Morton) order: a viewport's tiles then sit next to
   each other on disk and kernel readahead serves them almost free. Spatial
   clustering is a *disk-layout* optimisation, not just a bake optimisation.
2. **Keep the index off the heap.** A `HashMap` of Europe's ~3.4M tiles is
   ~100 MB of RAM — significant on a 1 GB box. Store the index as a sorted array
   of fixed-width `(key, offset, len)` records and mmap it too. Beware that a
   plain binary search over an 80 MB index is ~22 probes scattered across the
   whole file, which is several page faults on a cold cache; prefer a two-level
   layout (a small in-RAM directory of coarse cells, each pointing at a short
   sorted run) so a lookup costs 1–2 page faults. This is precisely what PMTiles
   does, which is a good reason to use it rather than reinvent it.

**Sizing.** Cumulative tile bytes by `MAXZOOM`, measured for Picardie and scaled
to Europe by the 262× PBF-size ratio:

| MAXZOOM | Picardie | Europe tiles | Europe raw | Europe gzipped |
| --- | --- | --- | --- | --- |
| 12 | 23.7 MB | ~245k | 6.2 GB | **4.2 GB** |
| 13 | 41.2 MB | ~889k | 10.8 GB | **7.2 GB** |
| 14 | 78.5 MB | ~3.4M | 20.6 GB | **13.8 GB** |

(Upper bounds: a real lower-`MAXZOOM` build also coarsens the size filter and
keeps fewer features, so it lands smaller than truncating a z14 build.)

Europe at z14 is ~14 GB gzipped, which does not comfortably share a 20–25 GB VPS
disk with an OS. Three ways out, in increasing order of how much they solve:

- **Cap `MAXZOOM` at 12** — 4.2 GB gzipped, fits anywhere, but no buildings.
- **Serve a region, not the continent** — the pipeline is per-country already.
- **Put the archive in object storage and skip the VPS.** Convert to
  [PMTiles](https://protomaps.com/docs/pmtiles) and host it on R2/B2/S3 behind a
  CDN: clients fetch tiles with HTTP range requests, egress is cached by the CDN,
  and there is no backend process, no RAM budget, and no disk to size. For an
  archive rebuilt monthly and never mutated, this is the best fit — and it makes
  the store-choice question above moot.

### Scaling to Europe

The pipeline is region-agnostic (`--regions europe`). Extraction *time* is no
longer the obstacle — the Rust extractor parallelises over blobs, so a 262×
larger file mostly means 262× more work spread over the same cores. What is left
is memory, and it is the reason a whole-Europe run is still not a thing you can
just start:

1. **Node cache.** The extractor holds one 16-byte record per node that some way
   it keeps refers to — 13.8M of Picardie's 15.2M nodes, or 220 MB. Europe is
   ~262× the PBF, so that is tens of GB and still does not fit. The structure is
   already the right one to fix it, though: it is a sorted array built in one
   pass and then only read, so it can be written to a file and mmapped without
   changing anything above it. That is the `dense_file_array` idea, minus the
   need to size it for every node in the planet.
2. **Way references.** Pass 2 holds the node ids of every way it keeps, which is
   the other unbounded structure and the larger one for dense extracts. Same
   remedy, and it has to happen at the same time.
3. **Tile count.** z14 over Europe is millions of tiles rather than thousands.
   At that point pre-baking every tile stops being the obvious choice, and either
   a lower `MAXZOOM` or on-demand baking (DuckDB can build a tile per request —
   the same `ST_AsMVT` call, just not stored) becomes the better trade.

Map data © OpenStreetMap contributors, ODbL.
