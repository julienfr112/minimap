# minimap

A small, working vector-tile map built from raw OpenStreetMap data. It defaults
to **Picardie** (one 133 MB extract) for fast iteration; `make all REGIONS=...`
scales it up to a country or the continent.

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
make            # what this is, how it is configured, where it got to
make all        # download -> load -> bake -> export
make serve      # http://127.0.0.1:8090
make test       # every test, in seconds, needing none of the above
make check      # cargo fmt --check and clippy with warnings as errors
```

The first build takes a few minutes, because `duckdb`'s `bundled` feature
compiles DuckDB itself — the payoff is that there is no system library to
install and no way for the spatial extension to be a version out of step.

Everything is a make variable, so nothing needs a source edit:

```bash
make all REGIONS="belgium netherlands luxembourg"   # a build named belgium+luxembourg+netherlands
make all REGIONS=france NAME=fr                     # ... or call it what you like
make all DUCKDB=/mnt/big/duckdb     # put the 154 GB somewhere it fits
make regions                        # what can go in REGIONS=
```

What the *map* is — the zoom rungs, the classes, the size thresholds — is not a
flag. It lives in `minimap_rs/src/tuning.rs`, and the Makefile depends on that
file, so editing it makes the stages stale and `make all` re-runs them. That is
the same guarantee a flag would give, with none of the machinery.

One directory per kind of thing, so what a directory holds is its name:

| | | |
| --- | --- | --- |
| `pbf/` | what was downloaded | expensive, polite, identical every rebuild |
| `duckdb/` | the database built from it | enormous, and pure scaffolding |
| `pmtiles/` | one archive per layer | **the deliverable** |
| `log/` | one file per stage | |

Each is a variable, which matters because they differ by three orders of
magnitude: `make all DUCKDB=/mnt/big/duckdb` puts the 154 GB where there is room
without moving the 135 MB off the machine that serves it. `make disk-stat`
says what each holds; `make clean-<category>` removes exactly one of `pbf`,
`duckdb`, `pmtiles`, `log`, `anon` or `binary` (cargo's `target/`) and nothing
else; `make clean` removes all of them, asking before the downloads.

Three maps have targets of their own, each just `make all` with the right
`REGIONS`:

```bash
make picardie        # the default region, a few minutes
make france          # one country
make europe          # download the 49 European extracts (31.7 GB), then build them all
```

To work at country granularity by hand, fetch the European extracts once and
name the ones you want:

```bash
make all REGIONS="belgium netherlands luxembourg"
make all REGIONS=    # ... or every extract present, which is the continent
```

### The whole continent

`make all REGIONS=` is the command. What it needs is the part worth reading
first, because the deepest rung sets the price and z17 is not z14:

| | Picardie | Europe, ~250× by extract bytes |
| --- | ---: | ---: |
| `duckdb/` | 1.1 GB | **~260 GB** |
| DuckDB spill, transient | — | **~100 GB** |
| `raw` for the largest country, transient | — | **~110 GB** |
| `pmtiles/` | 135 MB | **~33 GB** |
| wall clock | 4 m 40 s | **~1.5 days** |

Peak disk is not the sum: the load holds *all features plus one France's `raw`*
and drops it per region, and the spill belongs to the bake, after `raw` is gone.
Either way it wants **350–400 GB free**, and the archives are the only 33 GB of
that you keep.

It is resumable. The bake records each finished rung and picks up after the last
one, so an interrupted run costs the rung in flight rather than the day. Killing
it and re-running `make all REGIONS=` is safe.

Two ways to make it fit if it does not:

```bash
make all REGIONS= DUCKDB=/mnt/big/duckdb    # the 260 GB elsewhere, archives stay here
make all REGIONS="france belgium netherlands luxembourg germany"   # a subset
```

Lowering the deepest rung is the other lever, and by far the biggest: `ZOOMS` in
`tuning.rs` from `[10, 12, 15, 17]` to `[10, 12, 15]` divides the tile count by
four and the build by about the same. It costs the 192 m view, which is the whole
point of the deep rung, so it is a real trade rather than a free saving.

Downloads are resumable and re-runnable: a complete file is skipped by comparing
local size to the server's `content-length`, a partial one continues with a
`Range` request, and a transfer that dies is retried five times from wherever it
stopped. Four Geofabrik aggregates are deliberately excluded because they
overlap their own siblings and would be counted twice: `alps`, `dach`,
`britain-and-ireland`, and `united-kingdom` (which overlaps `great-britain` plus
Northern Ireland).

`make` is the interface; `minimap` is what it drives. The CLI takes every path
and zoom as an explicit flag and infers nothing from where its binary lives —
run `cargo run --release --bin minimap -- --help` to see it.

### Rungs, not a zoom range

The archive holds a handful of zoom levels — **rungs** — and the viewer reuses a
shallower tile drawn larger for everything in between. That works up to about
4×; past that the geometry, simplified to one pixel at *its* zoom, reads as
blurry. `ZOOMS` in `tuning.rs` is `[10, 12, 15, 17]`:

| stop | width at 1000 px | from |
| --- | --- | --- |
| z10 | 49 km | rung z10, native |
| z11 | 24.6 km | z10, 2× |
| z12 | 12.3 km | rung z12, native |
| z13 | 6.2 km | z12, 2× |
| z14 | 3.1 km | z12, 4× — the only weak one |
| z15 | 1.5 km | rung z15, native |
| z16 | 769 m | z15, 2× |
| z17 | 385 m | rung z17, native |
| z18 | 192 m | z17, 2× |

Four rungs across nine levels means a 4× stretch lands somewhere; z14 is between
the two views this map is for rather than inside either. The viewer's zoom is
discrete and bounded to exactly these stops.

Each rung is ~4× the tiles of the one above it, so the deepest is essentially the
whole cost. Picardie reaches full coverage of its bounding box at z14, so below
that it is exactly ×4: z10 is 270 tiles, z17 would be 1,042,860.

What keeps the deep end affordable is that the *areal* layers stop early.
`land` and `landuse` tile everything, so at z17 a million tiles would exist to
say "still farmland"; buildings and roads are sparse — they exist where people
do. `BACKGROUND_MAXZOOM = 12` caps the areal layers, and the viewer keeps that
rung's tiles underneath as the ground.

Two size thresholds, for the same reason:

| | | |
| --- | --- | --- |
| `MIN_PIXELS` | 3 | the honest visibility floor, and what the extractor keeps |
| `LANDUSE_PIXELS` | 12 | higher, because texture is not landmark |

Raising `MIN_PIXELS` instead would discard data: at 12 the deep rung loses
416,781 buildings, a quarter of them, all the ones under 4.6 m. Raising the
landuse threshold alone takes the wide view from 31,089 farmland polygons to
4,902 and costs no buildings at all. `ROAD_CLASSES` in the same file sets the
zoom each road class earns, which is the equivalent lever for lines.

### What gets built, and how to rebuild it

`make targets` lists every file the build produces and what it is for. The short
version:

| | |
| --- | --- |
| **`pmtiles/<name>.<layer>.pmtiles`** | **the deliverable.** One archive per layer. Copy these to the server; nothing else is needed at runtime. |
| `duckdb/<name>.duckdb` | scaffolding — `features`, `tile_layers`, `meta` |
| `duckdb/tmp/` | scaffolding — DuckDB's spill, 80+ GB at Europe scale |
| `log/<name>.<stage>.log` | one per stage |
| `duckdb/.load-<name>-…`, `.bake-…`, `pmtiles/.export-…` | which stages are done |
| `pbf/<region>.osm.pbf` | input — expensive, never auto-deleted |
| `pbf/land-polygons-split-3857.zip` | input — the coastline, which OSM has no ocean for |

`<name>` is the build's `NAME`, derived from `REGIONS` unless given: `picardie`,
`belgium+netherlands`, or `europe` when `REGIONS` is empty. Every file a build
writes carries it, so a Picardie test build sits beside the continent in the
same three directories and can never overwrite it:

```bash
make all                        # picardie.*.pmtiles, in a few minutes
make all REGIONS=               # europe.*.pmtiles, beside it
make serve NAME=picardie        # pick one; `make info` lists what is there
```

Each stamp lives **with the thing it describes** — load and bake write the
database, export writes the archives. So `rm -rf duckdb/` correctly makes `load`
pending again, and there is no way to hold a stamp claiming something exists when
it does not.

**To rebuild: just `make all`.** You should not normally need to clean anything first,
and this is the part worth understanding, because a full rebuild of Europe is
hours and most changes do not need one.

The stages depend on **`minimap_rs/src/tuning.rs`** — the rungs, the layers, the
classes, the size thresholds, the SQL derived from them. Editing it makes the
database stale and `make all` reloads. Editing anything else under `minimap_rs/`
deliberately does *not* invalidate: `extract.rs` and the `progress` crate change
how the work is done, not what comes out, and a performance fix should not cost
you an eight-hour bake.

`REGIONS` is part of the load's identity too, hashed into the stamp name.
A plain prerequisite could not do it: `features` is the union of the extracts,
but a country downloaded last month is *older* than a stamp written today, so
make would see nothing to do and leave a Europe request holding a Picardie map.

```bash
make all                                    # nothing to do, if nothing changed
make all REGIONS="picardie nord-pas-de-calais"   # load, bake, export
make all REGIONS=                           # every extract in pbf/
```

So cleaning is for two things only:

* **reclaiming disk** — the database dwarfs the archives (1.1 GB against 135 MB
  on Picardie; 154 GB against 15 GB on Europe), and once you have exported you
  do not need it until the next build. `make clean-duckdb` alone is enough, and
  leaves the deliverable in place; `make prune` is that plus the logs;
* **starting genuinely from scratch** — a suspected-corrupt database, or an
  interrupted build you would rather not resume.

```bash
make disk-stat                      # what is on the disk, by category, and what recovers each
make clean-duckdb clean-pmtiles     # from scratch, keeping the 31 GB of extracts
make all
make clean                          # everything, downloads and compiler output too (asks first)
```

Each `clean-<category>` touches its one directory and nothing else. `pbf/` is
the one that asks: re-downloading 31 GB from a free service to re-run a bake
would be both slow and rude.

### Watching a build

Every stage says what it is doing and how much longer it expects to take:

```
==> bake  1,034,854 features -> MVT tiles, z6..14
    z13  4,108 tiles                                               7.2s
    [########------------]  41%  z14 landuse band 4/64      33.4s elapsed, ~48s left
```

The bar is the current activity, the lines above it are the record. Percentages
come from a cost model per stage — bytes for a download, `4^zoom` plus a
per-zoom table scan for a bake, extract size for a load — so the estimate is
real arithmetic rather than a spinner, and wrong early while it calibrates. It
says `~` for a reason.

The same bar covers everything that runs long enough to wonder about: every
pipeline stage, `make anon`, `make working-set`, and the downloads. The one
thing outside it is the first compile, where cargo's own bar shows DuckDB
building; after that the binaries are rebuilt quietly and run directly.

Each stage also writes `log/<name>.<stage>.log`, via `--log` rather than a pipe.
That distinction matters: `| tee` makes the tool's stdout a pipe, and a pipe is
exactly how it decides there is no terminal to draw a bar on — so teeing would
silently trade the thing you are watching for the file you are not. Writing the
log directly lets the two audiences differ: a line that overwrites itself on
screen, and progress lines every 5% (plus a 30-second heartbeat, so an hour-long
band does not look like a hang) on disk.

The viewer takes a `#zoom/lat/lon` hash, so a view can be linked directly — for
the Saint-Leu quarter of Amiens:

    http://127.0.0.1:8090/#16/49.89870/2.30160

The backend exposes five routes, and the viewer needs nothing else:

| route | returns |
| --- | --- |
| `GET /` | the viewer shell, with `<base href>` set to wherever it was reached |
| `GET /minimap.js` | the viewer, compiled into the binary |
| `GET /meta.json` | TileJSON: zoom range, bounds, centre, and each layer's rungs |
| `GET /tiles/{layer}/{z}/{x}/{y}` | one layer's MVT tile, still gzipped; `204` if empty, `404` if no such layer, `304` on a matching etag |
| `GET`/`POST /zone` | the anonymity zone for a `lat`/`lon` (see `anon/README.md`); `404` until `make anon` has run |

`server/tests/routes.rs` asserts every one of those status codes, because the
viewer treats them as facts rather than as errors.

## How it works

**`minimap download`** fetches the extracts named by `REGIONS`, resolving each
name against Geofabrik's own published index, plus the coastline dataset that
OpenStreetMap does not contain. Transfers resume, retry and run concurrently.

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
tile grid, and `ST_AsMVT` encodes one blob per layer.

**`minimap export`** writes **one PMTiles archive per layer**. PMTiles is
`(z, x, y) -> blob` and has no concept of a layer, so this is the format's own
grain; the single multi-layer archive it replaced needed a trick, concatenating
per-layer protobufs because `Tile` is `repeated Layer layers = 3`. Splitting
deleted the merge step that produced that blob — the statement whose obvious
spelling held all 22 GB of Europe at once and died at 18.4 GiB — and halved the
bake's peak disk, since `tile_layers` and `tiles` no longer coexist. It also
gives each layer its own rung set and its own etag, so `land` stopping at z12
while `buildings` start at z15 needs no special case, and re-baking one layer
invalidates only that one. The price is a request per layer per tile.

**The backend** opens one mmap per layer and answers with a Hilbert id and two
binary searches. That is the whole runtime. Leaf directories are decoded on
first touch and kept in a per-archive cache of at most 128 MB -- bounded,
because Europe's `roads` alone has 85M directory entries and holding them all
decoded would be 1.9 GB of heap.

Each tile carries exactly one attribute per feature, `cls`, because that is all
the styling needs. Street names stay in the `features` table — the viewer draws
no labels, and carrying names would inflate every tile by roughly 40%.

**The frontend** (`web/minimap.js`) has no dependencies. It contains a ~50-line
protobuf reader, an ~80-line MVT decoder, and a canvas renderer. Because tile
coordinates arrive as integers on a 0..4096 grid, drawing is a single affine map
to screen pixels. The decoder is tested under node against tiles encoded by hand
(`web/minimap.test.js`); the renderer is checked by looking at it.

## Layout

```
Makefile           the interface: stages, their order, and what each `clean-*` removes
README.md          this
minimap_rs/        the build tool: download / load / bake / export / info / sql
  src/tuning.rs      what the map IS: layers, classes, thresholds, the SQL
  src/config.rs      what a RUN is: which directories, which name, which zooms
  src/download.rs    Geofabrik + coastline, resumable and concurrent
  src/extract.rs     .osm.pbf -> staging rows, on every core
  src/geom.rs        ring assembly, hole nesting, WKB
  src/rows.rs        the staging appender: one DuckDB column chunk at a time
  src/load.rs        raw -> features (classification SQL)
  src/bake.rs        features -> tile_layers (tiling SQL)
  src/export.rs      tile_layers -> one pmtiles archive per layer
  src/info.rs        `make info` -- what is in the build right now
  src/sql.rs         `minimap sql` -- ask the build database something
  attic/             programs that were needed once; see its README
progress/          the live bar and the ETA, shared by everything that runs long
server/            serves the archives: axum + mmapped PMTiles, no database
  README.md          embedding the map and/or the zones in your own axum app
  src/lib.rs         the router: five routes, and the archives behind them
  src/pmtiles.rs     the archive reader, and its bounded leaf cache
  web/index.html     page shell, compiled into the binary
  web/minimap.js     protobuf reader + MVT decoder + canvas renderer, likewise
  web/minimap.test.js  the decoder, under node
  tests/routes.rs    every status code the viewer relies on
  tests/cache_perf.rs  `make perf`: what the server's caches cost
  examples/working-set.rs  `make working-set`: how much has to stay in RAM
anon/              a separate service on the same `features` table

pbf/               downloaded. Only `clean-pbf` removes it, and it asks.
  *.osm.pbf          the extracts
  land-polygons-*    the coastline, which OSM does not have
duckdb/            the database, and DuckDB's spill. `clean-duckdb` removes it.
pmtiles/           one archive per layer -- the deliverable
log/               one file per stage, because these run for hours
```

The split between `tuning.rs` and `config.rs` is the one worth knowing: editing
the first changes the map, editing the second only changes where it lands.
Nothing in the pipeline reads an environment variable or derives a path from
`CARGO_MANIFEST_DIR` — every step takes a `&Config`, which is what makes the
build directory a flag rather than a rebuild.

## How it is tested

`make test` is the whole suite in seconds — 58 Rust tests and 6 under node,
none of which needs a download, a database, or a baked archive. `make check`
is `cargo fmt --check` plus clippy with warnings as errors. Both run
optimised and share one `target/` with the pipeline, because a debug build of
`bundled` DuckDB is a second four-minute compile that buys nothing.

| where | what it pins |
| --- | --- |
| `minimap_rs/src/geom.rs` | ring assembly, the shared-wall cancellation, hole nesting by containment, WKB byte layout |
| `anon/format/src/lib.rs` | the on-disk index the bake and the service must agree on byte for byte |
| `server/tests/routes.rs` | every status code the viewer branches on, and the `<base href>` under a nest |
| `server/src/pmtiles.rs` | Hilbert ids against the spec's worked values, directory round-trips, leaf eviction |
| `minimap_rs/src/tuning.rs` | the derivations — `min_span` against the tile geometry, road minzooms clamped to the rungs |
| `progress/src/lib.rs` | the formatters, and that the ETA declines to guess before it can |
| `server/web/minimap.test.js` | the protobuf reader and the MVT decoder, against tiles encoded by hand |
| `minimap_rs/src/bake.rs` | band partitioning: contiguous, complete, and one statement when the rung fits |
| `minimap_rs/src/extract.rs` | position packing, and reading a population out of `approx. 6,000` |

**Nothing under test reads an `.osm.pbf` or a baked archive.** The load's tests
run against in-memory DuckDB, the server's write valid PMTiles archives of
their own (`server/tests/fixture/mod.rs`), and the decoder's encode their tiles
by hand. That is what keeps the suite at seconds instead of the hours a real
build takes, which is the only reason it is worth running before every commit.

Two things are deliberately left out. **The renderer**, which is checked by
looking at it: a golden-image test of a canvas is a test of the font stack and
the GPU, and it fails on someone else's machine for reasons that have nothing
to do with the map. And **the geometry SQL**, which is checked through its
outputs rather than statement by statement — DuckDB's spatial functions are
not this repository's code, and a test that asserts what `ST_Simplify` returns
is a test of DuckDB's version.

`make perf` is the one thing `make test` skips, because it reports rather than
asserts: what a leaf-cache hit costs, what a 304 saves, RSS under sustained
load. It is a number to read, not a gate to pass — `make perf TILES=pmtiles`
runs it against the archives that actually shipped.

## Notes and limitations

**The rungs are z10, z12, z15, z17** (`ZOOMS` in `tuning.rs`). Past the deepest
the viewer overzooms once, reusing z17 scaled 2× to reach the 192 m view.

**The deepest rung is the main cost dial**, because the size filter is derived
from it. At z12 a tile spans ~6.3 km at latitude 50°, so a 10 m building is
~0.8 px and the filter discards essentially all of the ~1.8M buildings in the
extract — correct, but it means no buildings. z14 brings the threshold down to
~18 m, so buildings appear and a city quarter is legible; it also multiplies
the tile count by ~16 and is what makes the database interesting in size rather
than trivial. (These numbers were measured when the deepest rung was z14; the
rungs are now z10/12/15/17 and the arithmetic scales the same way.)

Because the filter runs during extraction, a database loaded for one deepest rung
does not contain what a deeper one would need — it would produce an archive that
is subtly, invisibly thin at its deepest zoom. That is why the rungs live in
`tuning.rs` and the stages depend on the file: changing them re-runs the load,
not just the bake.

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
(`minimap_rs/src/geom.rs`), because OSM `outer`/`inner` roles cannot be trusted and
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

Nothing in the database is read at serve time -- the archives are the whole of
what the server opens -- so the build database never needs to reach it.

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

**Sizing.** Cumulative tile bytes by deepest rung, measured for Picardie at an
earlier rung set and scaled to Europe by the 262× PBF-size ratio:

| deepest rung | Picardie | Europe tiles | Europe raw | Europe gzipped |
| --- | --- | --- | --- | --- |
| 12 | 23.7 MB | ~245k | 6.2 GB | **4.2 GB** |
| 13 | 41.2 MB | ~889k | 10.8 GB | **7.2 GB** |
| 14 | 78.5 MB | ~3.4M | 20.6 GB | **13.8 GB** |

(Upper bounds: a build with a shallower deepest rung also coarsens the size
filter and keeps fewer features, so it lands smaller than truncating a z14
build.)

Europe at z14 is ~14 GB gzipped, which does not comfortably share a 20–25 GB VPS
disk with an OS. Three ways out, in increasing order of how much they solve:

- **Stop the rungs at z12** — 4.2 GB gzipped, fits anywhere, but no buildings.
- **Serve a region, not the continent** — the pipeline is per-country already.
- **Put the archive in object storage and skip the VPS.** Convert to
  [PMTiles](https://protomaps.com/docs/pmtiles) and host it on R2/B2/S3 behind a
  CDN: clients fetch tiles with HTTP range requests, egress is cached by the CDN,
  and there is no backend process, no RAM budget, and no disk to size. For an
  archive rebuilt monthly and never mutated, this is the best fit — and it makes
  the store-choice question above moot.

### How many users a small box holds

The sizing above asks what fits on the disk. What decides the box is what fits
in *RAM*, and that depends on something the archive size cannot tell you:
traffic lands on a few city centres, never evenly on the map.

`make working-set` measures that, against a real build rather than a model. For
a 27 GB Europe archive at rungs 10/12/15/17:

| | |
| --- | ---: |
| every rung up to z12, all six layers — shared by everyone, everywhere | 2.12 GB |
| 24 European capitals: z17 over 12×12 km plus z15 over 48×48 km | 435 MB |
| **resident once every centre is warm** | **2.56 GB** |

Paris is the worst single city at 41 MB. Across all 24, buildings are 61% of
the city bytes and roads 36%, which is also the order to cap things in if it
ever does not fit — the tool prints that split every time it runs. So a
4-core, 8 GB box holds the whole hot set with ~5 GB to spare, and `make perf`
says what that buys: **1.8 µs** for a warm tile against **714 µs** for one that
reaches the disk.

Concentration is doing all the work. Traffic spread evenly over Europe at z17
would want all 27 GB resident, every request would miss, and the same box would
serve two orders of magnitude fewer people. A map that everyone looks at in the
same twenty cities is the easy case, and it is also the usual one.

The process itself stays small whatever the traffic does: its one heap
structure of any size is the decoded-directory cache in `server/src/pmtiles.rs`,
capped at 128 MB per archive, so six layers can never hold more than 768 MB
however far a client walks, and only the three deep layers can approach it.
That cap exists because the alternative was measured -- every directory entry
of Europe's six archives, decoded, is 3.1 GB -- and `make perf TILES=pmtiles`
checks it holds against the archives that shipped.

**What one viewer costs** — measured, except where noted:

| | |
| --- | ---: |
| first screenful, cold browser | 64–255 kB, ~140 kB typical |
| a pan or a zoom step | ~30–50 kB; the client keeps 400 decoded tiles |
| a map on screen that nobody is touching | nothing — no session, no polling, no socket to hold |
| a return visit inside 7 days | a few 304s (`max-age=604800`, one etag per archive) |

**Where the ceiling is.** Measured over a real socket rather than derived from
the per-tile cost: the release binary pinned to **four cores** serves
**150 000 tiles/s at 700 Mbit/s** on a warm city working set — mean 52 µs at 8
connections, 208 µs at 32, 3.5 ms at 512, and no errors anywhere in that range.
Pinned to **two cores** it serves **96 000 at 448 Mbit/s**. Throughput is flat
from 8 connections to 512, so saturation costs latency rather than failures,
and RSS is 25 MB after five million requests. Halve it again for TLS in the
same process, though normally a proxy terminates that.

Against those, the tiles/s per user below is the one estimated column —
everything else here was measured:

| behaviour | tiles/s each | 4 cores | 1 Gbit uplink | 100 Mbit uplink |
| --- | ---: | ---: | ---: | ---: |
| map open, occasional interaction | ~0.5 | 300 000 | 100 000+ | ~20 000 |
| browsing — a view change every ~8 s | ~4 | **37 000** | ~20 000 | **~2 000** |
| continuous panning | ~45 | ~3 300 | ~2 700 | ~270 |

The `4 cores` column is that 150 000 divided by the row; the uplink columns are
unchanged and are what binds first on anything under a gigabit. That is the
useful conclusion and it did not change when the ceiling was measured: **the
network is the wall, not this process.**

So: on a gigabit link, ~20 000 concurrently *browsing* users and a couple of
thousand panning continuously — both of those are the uplink's number, not the
processor's, which is 37 000 and 3 300. With the map merely on screen it is
hundreds of thousands and the question stops being interesting. On 100 Mbit the
uplink is the wall long before this server is, by a factor of ten.

Three things bite before the tile path does:

1. **Sockets, not tiles.** 4 000 browsers at ~6 HTTP/1.1 connections each is
   24k sockets — raise `ulimit -n`, and prefer HTTP/2 so it is one connection
   per user. Their kernel buffers are the only thing here that grows with user
   count; the tile path itself measures **0 bytes of RSS per request** over
   200 000 requests.
2. **Cold start.** After a restart the first request to each city pays that
   714 µs. It is survivable — a screenful is ~12 ms extra, spread over parallel
   connections — but replay a list of city tiles at boot if the first minute
   matters.
3. **Storage.** All of this assumes local NVMe. On network-attached VPS disk a
   cold tile is 5–10 ms, which turns warm-up from a wrinkle into a stall, and
   makes the point above mandatory rather than optional.

And the cheapest fix for all three, if the traffic ever justifies it: a CDN in
front. Tiles are immutable within a build and already carry the etag and the
week-long `max-age` that make caching them correct.

### Scaling past Europe

Europe is built -- "The whole continent" above has the numbers -- and the two
structures that were expected to stop it did not: the extractor's node array
and way references are sized per region, and a region at a time is what the
load does. What still scales badly is the tile count at the deepest rung, and
the lever for that is in `tuning.rs`, not in the code: each rung is ~4x the
one above it, so dropping z17 divides the build by about four. Baking on
demand (DuckDB can produce a tile per request with the same `ST_AsMVT` call,
just not stored) is the other trade, and the one this design deliberately does
not make, because it would put the database on the serving machine.

Map data © OpenStreetMap contributors, ODbL.
