# anon

Turn a position into a zone that does not name the building it is in — and make
the zone bigger where that takes more room.

A parallel service to the tile pipeline, built on the same idea and the same
`features` table: everything geometric happens once, offline, and what is left to
serve is a lookup.

```
DuckDB `features`  --anon-bake-->  anon-zones.bin  --anon-serve-->  HTTP
236.8M buildings    Hilbert cut     15.8M zones     binary search    one JSON
90s, 3 tiers of k   4.5 B a zone    + one block scan no geometry code
                    70.3 MB, all of Europe
```

The lookup is one function over one file — [`Index::zone`] — and the file is the
compressed form, not an archive that gets inflated first. Nothing else is
consulted: no database, no geometry library, no decompression pass.

[`Index::zone`]: format/src/lib.rs

```bash
make anon           # cut the zones from the pipeline database (35 s over Europe)
make anon-serve     # the standalone service, http://127.0.0.1:8091
curl -d 'lat=49.8949&lon=2.3020' localhost:8091/zone
```

Flags go through `ANON_FLAGS` (`make anon ANON_FLAGS='--min-footprint 25'`);
`anon-bake --help` lists them. The map server picks the index up too: after
`make anon`, `make serve` grows a `/zone` endpoint and the viewer lets you
**click a point to see its zone** — the exact cells, filled; not the bbox,
which is a bound and reads twice as big — because the honest way to explain
"you are one of 74 buildings in here" is to draw the here.

```json
{"zone":"3665e63eb4","k":64,"buildings":74,
 "bbox":[2.300262,49.894634,2.304382,49.896404],"center":[2.302322,49.895519],
 "radius_m":178,"area_km2":0.039,"cells":16,
 "density_per_km2":3136,"built_index":49.78,"kind":"city"}
```

`POST` because a URL ends up in access logs, browser history, `Referer` headers
and cache keys by default, and the URL is the thing being protected. `GET
/zone?lat=&lon=` works too, for curl and for people.

## What the answer means

The position is at one of **`buildings` buildings inside `bbox`**, and that is
all the caller learns. `k` is the floor the operator asked for; `buildings` is
what the zone actually holds, which is at least `k` and often more.

`radius_m` is how vague the answer is — how far from `center` the position could
be. `area_km2` and `cells` are the anonymity set's true size, not a bound.
`built_index`, `density_per_km2` and `kind` are what kind of place it is. Those
are separate measurements on purpose: radius scales as `sqrt(k / density)`, so it
says as much about the operator's choice of `k` as about the place.

`kind` comes from built *area*, not from the building count, because the count
gets city centres wrong: Charing Cross and Berlin Mitte have few buildings per km²
and they are enormous, so by count they read as thinner than a housing estate in
Massy. Both numbers are reported, because where they disagree is interesting.

`built_index` is buildings' total bounding-box area over ground area, as a
percentage — an *index*, not a coverage fraction, which is why it isn't called
one: bounding boxes overlap in dense fabric, so central Paris comes out at 114.
A true coverage percentage means reading `geom` for 121M polygons, two orders of
magnitude more work than the four numbers the bake already reads. Worth it for a
number a caller displays; not for a six-way label whose thresholds are calibrated
on this same measure.

`bbox` is a *bound*, and in open country a loose one: sixty-four buildings out
there may be three hamlets apart, so the box spans the gap. It does always contain
the position that asked, which is the one thing a box drawn around the zone's
*buildings* would not — a position standing in a field between them falls outside
that. `area_km2` is the zone itself and not the box, so the two differ by however
much the curve wandered.

The map server's `/zone` answers with one more field: `quads`, the zone's cells
as `[w,s,e,n]` boxes (a few dozen at most — a zone is an interval of the curve,
and an interval decomposes into aligned squares). That is the zone's true shape,
and what the viewer fills in when you click. It is still a function of the zone
alone, so it discloses nothing the zone id did not.

## Why not just round the coordinates

Because a fixed precision is a fixed area, and area is not privacy. Three
decimal places is 110 m: a city block in Lille, and one farm on the Causse
Méjean — where it does not anonymise the farm, it *names* it. Only density can
set the size, which is why this needs baked data and cannot be arithmetic on the
coordinates.

## Why not add noise

A random offset per request is the other obvious answer, and it leaks. `n`
independent draws around one true position converge on it as `1/sqrt(n)`, so a
service that jitters afresh each call hands the building to anyone who asks five
hundred times. Draw the noise once per user and it stops being noise and becomes
a fixed offset — a grid, secretly, and one whose guarantee nobody has checked.

This is deterministic instead: the same building always yields the same zone, so
asking again learns nothing. Planar Laplace (geo-indistinguishability) is the
principled version of the noise approach and is worth reading if you want a
differential-privacy bound rather than a k-anonymity one; it needs per-user state
and a privacy budget, which is a different service than this one.

## How the zones are built

Buildings are binned on a fixed Web Mercator grid at z19 (51 m cells at
Amiens), the occupied cells are put in **Hilbert order**, and the curve is cut
wherever the running building count reaches `k` — at the most coarsely aligned
key that costs the zone nothing extra, so boundaries land on quadtree corners
where the curve grows in blocks rather than ribbons (see `cut` for the two
rules that keep this free). A zone is one interval of the curve. A lookup is:
project, bin, Hilbert, one binary search.

Two properties make it safe, and both are easy to lose:

* **It is a partition.** Every position belongs to exactly one zone, and every
  position in a zone gets the same answer — so the set of positions that could
  have produced an answer *is* the zone, which holds `k` buildings.
* **Every field of the response is a function of the zone alone.** The zone id
  is all the caller learns about the position. A `contains_query` flag, a
  distance to the centre, a bbox clipped to the query's half of the zone: each
  hands back a bit the zone id does not contain.

The obvious alternative — an adaptive quadtree, descend while the cell still
holds `k` buildings — is **wrong**, and quietly. If the answer is a z12 cell
because the z13 child holding the position was too sparse, then the answer
discloses *which* child: the sparse one, which has fewer than `k` buildings by
construction. Requiring all four children to clear `k` before splitting fixes the
leak and destroys the resolution instead — one empty quadrant of farmland blocks
every refinement above it, so the city next door inherits a 40 km cell. Cutting
a space-filling curve has neither problem, and needs no special case at the city
edge.

Hilbert rather than Morton because a zone is an interval of the curve and is only
compact if the curve is local: Morton jumps across the world at every
power-of-two boundary, and those jumps would land in the bbox.

## Why the database is small

Because a zone *is* an interval, the index never stores a zone — only where each
one starts. The geometry follows from the pair of breakpoints around it: the
shape is the set of grid cells the interval covers, recovered exactly by
decomposing the interval into aligned quadtree squares (a Hilbert curve visits
every such square contiguously, so an aligned run of `4^j` keys *is* a square of
side `2^j`; a test checks the derived box against walking every cell, for every
interval of every grid up to 16×16).

That is 16 bytes a zone not written down, and it also removes the one dishonest
thing about storing a box: a box around the zone's *buildings* does not contain a
position standing in a field between them. The interval always does.

What is left per zone — a breakpoint, a count, and two quantised bytes — is delta
coded in blocks of 64 against a sampled skip table. A lookup binary-searches the
skip entries and scans one block: a few hundred contiguous bytes, whatever the
index weighs. Over Europe that comes to **4.5 bytes a zone**, 70.3 MB for three
tiers — against the 16 bytes a breakpoint alone would cost stored plainly.

The cost of a lookup, on a synthetic continent so it needs no database (a
sparser one than Europe, hence the larger bytes-per-zone):

```bash
cargo run --release -p anon-format --example cost
250280 zones, 1.5 MB, 6.14 bytes a zone
1265 ns a lookup, of which ~427 ns rebuilding the geometry
```

1.3 µs is slower than reading a fixed-size record would be, and worth being
straight about: a third of it is arithmetic rebuilding the geometry, not memory.
790k lookups a second on one core was never the constraint; resident memory was.

|  |  |
|---|---|
| breakpoint | varint delta from the previous zone, restarted each block |
| buildings | varint; the count itself, not the overshoot above `k`, so the one zone allowed to fall short cannot read back as exactly `k` |
| built-up % | one byte, square-rooted: 0.002% resolution at the bottom of the range where wilderness and farmland part company, 0.4% at the top where a city centre does not care |
| buildings/km² | one byte, same trick, ~3% steps |

The alternative reading of "compressed" — gzip the 318 MB file and inflate it at
startup — buys nothing: you would still hold 318 MB resident to answer from it.
This is smaller *in memory*, which is the number that decides whether the whole
of Europe ships inside an application or beside one.

## Choosing k

`anon-bake` prints what each `k` buys, by the kind of place it buys it in, which
is the table to decide from — a single median over all zones hides the point,
since the countryside contributes most of the zones and almost none of the
positions. Over Europe:

```
k=64: 3 104 759 zones
  city-centre     82 591 zones   radius p50    148 m   p90    311 m
  city           424 472 zones   radius p50    171 m   p90    354 m
  suburb         793 617 zones   radius p50    212 m   p90    504 m
  village        897 955 zones   radius p50    321 m   p90    974 m
  countryside    902 174 zones   radius p50   1060 m   p90   2761 m
  wilderness       3 950 zones   radius p50   5491 m   p90  13374 m
```

The ladder is calibrated against landmarks, which is the only test it can be held
to — and the one that moved these thresholds twice:

| | `built_index` | | `built_index` |
|---|---|---|---|
| Lofoten | 0.4 | Amiens cathedral | 35 |
| Villers-Bocage | 1.1 | Créteil | 41 |
| Chamonix | 4.2 | Charing Cross | 55 |
| Dury | 9.3 | Berlin Mitte | 76 |
| Massy | 22 | Châtelet | 114 |

Several `k` live in one index so that changing it is a config change rather than
a re-bake. **It is not a query parameter**, and that is deliberate: tiers nest, so
a position's k=16 zone sits inside its k=256 zone, and letting the caller choose
means anyone who asks twice keeps the smaller answer. One `k` per deployment
(`ANON_K`); run a second instance if a second use case needs a different one.

## What the guarantee does not cover

* **Buildings are a proxy for people.** A hamlet of three houses and thirty barns
  counts as thirty-three, so a `k=32` zone there could be one family. This is the
  weakness that matters, and it is one flag: `--min-footprint 25` drops sheds.
  Baking against address points or a population grid instead is better still and
  changes nothing downstream — the count per cell is all the bake wants from the
  data. A 200-flat tower counting once errs the safe way.
* **A trajectory is not a position.** Zones are stable, so a sequence of them is
  a path at zone resolution, and a home zone plus a work zone identifies very
  few people. Anonymising each point of a trace is not anonymising the trace.
* **The service must not log the request line.** `lat`/`lon` are the thing being
  protected; an access log is a durable, greppable, backed-up record of exactly
  what the API exists to avoid passing on. `anon-serve` writes nothing, and the
  proxy in front of it has to be configured not to either — which is one line,
  and the one piece of this design that lives outside the repository:

  ```nginx
  location /zone {
      access_log off;                    # the whole point
      proxy_pass http://127.0.0.1:8091;
  }
  ```

  (Caddy: `log_skip` on the route.) Answers are `Cache-Control: private` for
  the same reason — a CDN entry keyed on a lat/lon URL is that log by another
  name.
* **Coverage is a disclosure.** A position outside the baked region gets a 404
  rather than the nearest zone on the wrong continent, which does say "outside
  Europe" about it. Deliberate, coarse, and the one documented exception to the
  rule above.

## Files

| | |
|---|---|
| `format/` | The ~450 lines the two ends agree on byte for byte: Hilbert, the file layout, the lookup, and what a `Zone` is allowed to say. No dependencies. |
| `bake/` | Reads `features`, cuts the curve, writes the index. Wants DuckDB. |
| `serve/` | mmaps the index and answers `/zone`. Wants axum and 200 lines. |

The map server (`../server/`) embeds the same lookup for the viewer's click,
so the map needs no second process; `serve/` is for deploying the answer
*without* the map.
