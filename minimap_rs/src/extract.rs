//! .osm.pbf to staging rows — the job libosmium does, done here.
//!
//! Reads one Geofabrik extract and pushes batches of `raw` rows at a [`Sink`].
//! Nothing here classifies or reprojects anything: this step's entire output is
//! "a drawable object, its handful of tags, and its WKB in WGS84". DuckDB does
//! the rest in SQL.
//!
//! **Why it is written out rather than delegated.** The first version of this
//! pipeline called pyosmium, and that loop was ~95% of load time and single
//! threaded, because every OSM object crossed into Python to be looked at. Here
//! the file is decoded on every core and an object becomes a row without ever
//! becoming an object.
//!
//! **Three passes, not one.** A PBF stores nodes, then ways, then relations, so
//! a single pass cannot resolve anything: a way's geometry needs node
//! coordinates it has already scrolled past, and a relation's needs ways it has
//! already scrolled past. libosmium solves this by holding *every* node
//! location in RAM (`flex_mem`), which is what stopped the old pipeline from
//! reaching Europe. Reading the file three times instead is the better trade --
//! decompression parallelises, and we learn which nodes matter before we store
//! any:
//!
//!   1. relations -- which multipolygons do we want, and which ways do they need
//!   2. ways      -- keep the ones we draw or a wanted relation needs; note their nodes
//!   3. nodes     -- coordinates, but only for the nodes step 2 asked for
//!
//! For Picardie that stores 13.8M of the file's 15.2M node locations, which is
//! a thin saving -- at MAXZOOM=14 we keep buildings, and buildings touch nearly
//! every node there is. The ratio is not the point. The point is that the set
//! is known before a single coordinate is stored, so it can be a flat sorted
//! array of exactly the right length at 16 bytes per node, instead of a
//! general-purpose sparse map sized for whatever might turn up.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use osmpbf::{BlobDecode, Mmap, MmapBlob, PrimitiveBlock};
use rayon::prelude::*;

use crate::progress;
use crate::geom::{self, Pt, Ring};
use crate::rows::{Interner, Row, Sink, TagSet, Tags, BATCH};

/// Waterways worth drawing as a line. Everything else tagged `waterway` is
/// either an area or too small to matter.
const WATERWAY_LINES: [&str; 5] = ["river", "canal", "stream", "ditch", "drain"];

/// A closed way with any of these is a candidate area.
const AREA_KEYS: [&str; 6] = [
    "building", "landuse", "natural", "leisure", "water", "waterway",
];

/// No coordinate stored for this node yet. A real node at exactly
/// (-1e-7, -1e-7) would be indistinguishable; it is 11 metres off Null Island
/// and it does not exist.
const MISSING: u64 = u64::MAX;

/// A named settlement, for labels.
#[derive(Clone)]
pub struct Place {
    pub name: String,
    pub kind: String,
    pub population: i64,
    pub lon: f64,
    pub lat: f64,
}

/// Settlements worth a label. `city` and `town` only: `village` and below would
/// multiply the count by an order of magnitude to write names nobody reads at
/// the zooms this map offers.
const PLACE_KINDS: [&str; 2] = ["city", "town"];

/// Pass 4, and the only one that treats a node as a feature in its own right.
///
/// Kept out of the way/area machinery entirely rather than threaded through
/// `TagSet` as two more columns. That structure is the extractor's largest --
/// one per candidate object, and Belgium alone has 10.5M of them -- so widening
/// it to carry tags that only nodes use would cost every way in Europe to serve
/// a few tens of thousands of cities. This walks the same mmapped blobs and
/// returns a Vec small enough to hand around whole.
pub fn scan_places(path: &Path) -> Result<Vec<Place>, Box<dyn std::error::Error>> {
    let mmap = unsafe { Mmap::from_path(path)? };
    let blobs: Vec<MmapBlob<'_>> = mmap.blob_iter().collect::<osmpbf::Result<_>>()?;
    let places: Vec<Place> = blobs
        .par_iter()
        .filter_map(|blob| blob.decode().ok())
        .flat_map_iter(|block| {
            let mut out = Vec::new();
            if let osmpbf::BlobDecode::OsmData(block) = block {
                for node in block.groups().flat_map(|g| g.nodes()) {
                    collect_place(node.tags(), node.lon(), node.lat(), &mut out);
                }
                for node in block.groups().flat_map(|g| g.dense_nodes()) {
                    collect_place(node.tags(), node.lon(), node.lat(), &mut out);
                }
            }
            out
        })
        .collect();
    Ok(places)
}

fn collect_place<'a>(
    tags: impl Iterator<Item = (&'a str, &'a str)>,
    lon: f64,
    lat: f64,
    out: &mut Vec<Place>,
) {
    let (mut name, mut kind, mut population) = (None, None, 0i64);
    for (k, v) in tags {
        match k {
            "name" => name = Some(v.to_string()),
            "place" if PLACE_KINDS.contains(&v) => kind = Some(v.to_string()),
            // Freeform in practice: "1 234 567", "approx 40000", "40,000".
            // Keep the digits and give up on the rest rather than drop a city
            // for spelling its size oddly.
            "population" => {
                let digits: String = v.chars().filter(char::is_ascii_digit).collect();
                population = digits.parse().unwrap_or(0);
            }
            _ => {}
        }
    }
    if let (Some(name), Some(kind)) = (name, kind) {
        out.push(Place {
            name,
            kind,
            population,
            lon,
            lat,
        });
    }
}

pub fn run(
    path: &Path,
    sink: &mut impl Sink,
    min_span: f64,
    label: &str,
    weight: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    // Mapping the file gives every worker a cheap view of it; the blob list is
    // just offsets and lengths into that map, so the three passes below re-read
    // the file without re-scanning its framing.
    let mmap = unsafe { Mmap::from_path(path)? };
    let blobs: Vec<MmapBlob<'_>> = mmap.blob_iter().collect::<osmpbf::Result<_>>()?;

    // One vocabulary for the whole extract, shared by both scanning passes.
    let words = Interner::default();

    // How the region's weight divides across the six phases below. Roughly
    // measured rather than derived: the three decode passes dominate, and the
    // node pass is the one that also has a sort in it.
    let (w_rel, w_way, w_node) = (weight * 0.15, weight * 0.30, weight * 0.30);
    let (w_lines, w_areas, w_multi) = (weight * 0.05, weight * 0.15, weight * 0.05);

    let relations = scan_relations(&blobs, &words, &format!("{label} pass 1/3 relations"), w_rel);
    let wanted: HashSet<i64> = relations
        .iter()
        .flat_map(|r| r.members.iter().copied())
        .collect();

    let mut ways = scan_ways(&blobs, &wanted, &words, &format!("{label} pass 2/3 ways"), w_way);
    ways.refs.index.sort_unstable_by_key(|s| s.way);

    let locations = scan_nodes(&blobs, &ways.refs, &format!("{label} pass 3/3 nodes"), w_node);
    progress::line(format!(
        "  {} ways kept, {} multipolygons, {} node locations",
        progress::commas(ways.refs.len() as u64),
        progress::commas(relations.len() as u64),
        progress::commas(locations.ids.len() as u64),
    ));

    let (mut n_lines, mut n_way_areas, mut n_rel_areas) = (0, 0, 0);

    progress::at(format!("{label} building lines"));
    let n_line_chunks = ways.lines.chunks(BATCH).count().max(1);
    for chunk in ways.lines.chunks(BATCH) {
        let batch: Vec<Row> = chunk
            .par_iter()
            .filter_map(|(id, tags)| build_line(*id, tags.resolve(&words), &ways.refs, &locations))
            .collect();
        n_lines += batch.len();
        sink.write("line", &batch)?;
        progress::tick(w_lines / n_line_chunks as f64);
    }
    let n_area_chunks = ways.areas.chunks(BATCH).count().max(1);
    for (c, chunk) in ways.areas.chunks(BATCH).enumerate() {
        progress::at(format!("{label} building areas {}/{n_area_chunks}", c + 1));
        let batch: Vec<Row> = chunk
            .par_iter()
            .filter_map(|(id, tags)| {
                // A lone closed way has no second ring to share a wall with, so
                // it goes straight to a ring without the assembly step.
                let ring = resolve(ways.refs.get(*id)?, &locations)?;
                // Area ids follow libosmium's convention so the two sources
                // cannot collide: a way-area is 2*id, a relation-area 2*id+1.
                build_area(2 * *id, tags.resolve(&words), vec![ring], min_span)
            })
            .collect();
        n_way_areas += batch.len();
        sink.write("area", &batch)?;
        progress::tick(w_areas / n_area_chunks as f64);
    }
    progress::at(format!("{label} assembling multipolygons"));
    let n_rel_chunks = relations.chunks(BATCH).count().max(1);
    for chunk in relations.chunks(BATCH) {
        let batch: Vec<Row> = chunk
            .par_iter()
            .filter_map(|rel| {
                // A member way missing from the file, or a node missing under
                // one, means the relation is cut by the extract boundary. Half
                // a coastline is worse than none.
                let parts: Vec<Vec<u64>> = rel
                    .members
                    .iter()
                    .map(|w| resolve(ways.refs.get(*w)?, &locations))
                    .collect::<Option<_>>()?;
                let rings = geom::assemble_rings(parts)?;
                build_area(2 * rel.id + 1, rel.tags.resolve(&words), rings, min_span)
            })
            .collect();
        n_rel_areas += batch.len();
        sink.write("area", &batch)?;
        progress::tick(w_multi / n_rel_chunks as f64);
    }

    progress::line(format!(
        "  {} lines, {} areas ({} ways + {} relations)",
        progress::commas(n_lines as u64),
        progress::commas((n_way_areas + n_rel_areas) as u64),
        progress::commas(n_way_areas as u64),
        progress::commas(n_rel_areas as u64),
    ));
    Ok(())
}

// --- passes ---------------------------------------------------------------

/// Decode every data blob on all cores and fold the results together.
///
/// Blobs are independent zlib streams, which is the whole reason this is worth
/// parallelising: decompression is the cost of a pass, and it divides cleanly.
fn scan<T: Send>(
    blobs: &[MmapBlob<'_>],
    pass: &str,
    weight: f64,
    empty: impl Fn() -> T + Sync + Send,
    visit: impl Fn(&PrimitiveBlock, &mut T) + Sync + Send,
    merge: impl Fn(T, T) -> T + Sync + Send,
) -> T {
    // Blobs are the only unit of this that is countable before it happens, and
    // there are thousands of them per extract, so they are what the bar moves
    // on. Without this a pass over France is four silent minutes.
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let n = blobs.len().max(1);
    let each = weight / n as f64;
    blobs
        .par_iter()
        .fold(&empty, |mut acc, blob| {
            if let BlobDecode::OsmData(block) = blob.decode().expect("corrupt PBF blob") {
                visit(&block, &mut acc);
            }
            let i = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // Every blob ticks the weight, but only every 64th rewrites the
            // label: formatting a string per blob would cost more than the
            // decode on the small ones.
            progress::tick(each);
            if i.is_multiple_of(64) || i == n {
                progress::at(format!("{pass} {i}/{n}"));
            }
            acc
        })
        .reduce(&empty, merge)
}

struct RelArea {
    id: i64,
    tags: TagSet,
    members: Vec<i64>,
}

/// Pass 1: multipolygon and boundary relations carrying a tag we draw.
fn scan_relations(blobs: &[MmapBlob<'_>], words: &Interner, pass: &str, weight: f64) -> Vec<RelArea> {
    scan(
        blobs,
        pass,
        weight,
        Vec::new,
        |block, out: &mut Vec<RelArea>| {
            for group in block.groups() {
                for rel in group.relations() {
                    let mut kind = "";
                    let mut wanted = false;
                    for (k, v) in rel.tags() {
                        if k == "type" {
                            kind = v;
                        }
                        wanted |= AREA_KEYS.contains(&k);
                    }
                    if !wanted || (kind != "multipolygon" && kind != "boundary") {
                        continue;
                    }
                    // A relation member would need its own assembly pass; the
                    // construct is deprecated and rare, so skip rather than
                    // silently emit the outline without its parts.
                    let mut members = Vec::new();
                    let mut nested = false;
                    for m in rel.members() {
                        match m.member_type {
                            osmpbf::RelMemberType::Way => members.push(m.member_id),
                            osmpbf::RelMemberType::Relation => nested = true,
                            osmpbf::RelMemberType::Node => {}
                        }
                    }
                    if nested || members.is_empty() {
                        continue;
                    }
                    out.push(RelArea {
                        id: rel.id(),
                        tags: area_tags(rel.tags(), words),
                        members,
                    });
                }
            }
        },
        |mut a, b| {
            a.extend(b);
            a
        },
    )
}

#[derive(Default)]
struct Ways {
    lines: Vec<(i64, TagSet)>,
    areas: Vec<(i64, TagSet)>,
    /// Node ids of every way the two lists above need.
    refs: WayRefs,
}

/// Where one way's node ids live.
#[derive(Clone, Copy)]
struct Span {
    way: i64,
    chunk: u32,
    start: u32,
    count: u32,
}

/// Node ids of many ways: an `index` sorted by way id, over per-worker `chunks`
/// that hold the ids end to end.
///
/// Two things are deliberate here. A `Vec<(i64, Vec<i64>)>` would cost a heap
/// allocation per way, and a dense country has millions of them -- Belgium's
/// 10.5M ways each paid a `Vec` header plus a size-class-rounded allocation on
/// top of the ids. So ids go end to end and a way costs 24 bytes of index.
///
/// And the chunks are never concatenated into one buffer, which is what makes
/// merging cheap. Appending one worker's ids to another's means reallocating a
/// buffer that already holds hundreds of millions of them, so old and new exist
/// at once: on France that transient alone was 4.2 GB, more than a tenth of the
/// machine. Moving the buffers instead costs 24 bytes per chunk and there are a
/// few hundred of them. Sorting likewise touches only the index, so ids are
/// never permuted either.
#[derive(Default)]
struct WayRefs {
    index: Vec<Span>,
    chunks: Vec<Vec<i64>>,
}

impl WayRefs {
    fn len(&self) -> usize {
        self.index.len()
    }

    /// The node ids of one way. `None` if this extract does not contain it.
    ///
    /// A way's ids are always contiguous within one chunk, because the worker
    /// that read the way appended them in one go.
    fn get(&self, id: i64) -> Option<&[i64]> {
        let k = self.index.binary_search_by_key(&id, |s| s.way).ok()?;
        let s = self.index[k];
        let chunk = &self.chunks[s.chunk as usize];
        Some(&chunk[s.start as usize..s.start as usize + s.count as usize])
    }

    /// Total ids held, for sizing pass 3's array exactly.
    fn total(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }
}

/// Pass 2: the ways we draw, plus the ways the wanted relations are built from.
fn scan_ways(blobs: &[MmapBlob<'_>], wanted: &HashSet<i64>, words: &Interner, pass: &str, weight: f64) -> Ways {
    scan(
        blobs,
        pass,
        weight,
        Ways::default,
        |block, out: &mut Ways| {
            for group in block.groups() {
                for way in group.ways() {
                    let id = way.id();
                    let mut highway = None;
                    let mut waterway = None;
                    let mut area_key = false;
                    // `area=no` is how a mapper says "this loop is a loop, not a
                    // surface" -- an athletics track, a ring road. Without it we
                    // would fill them in.
                    let mut area_no = false;
                    for (k, v) in way.tags() {
                        match k {
                            "highway" => highway = Some(v),
                            "waterway" => waterway = Some(v),
                            "area" => area_no = v == "no",
                            _ => {}
                        }
                        area_key |= AREA_KEYS.contains(&k);
                    }
                    let is_line =
                        highway.is_some() || waterway.is_some_and(|w| WATERWAY_LINES.contains(&w));

                    // Append first and inspect in place, so a way that turns out
                    // to be undrawable costs no allocation at all -- only a
                    // truncate back to where it started.
                    if out.refs.chunks.is_empty() {
                        out.refs.chunks.push(Vec::new());
                    }
                    let buf = out.refs.chunks.last_mut().expect("just pushed");
                    let start = buf.len();
                    buf.extend(way.refs());
                    // libosmium only builds areas from closed ways, and a ring
                    // needs three corners plus the repeated first node.
                    let refs = &buf[start..];
                    let is_area =
                        area_key && !area_no && refs.len() >= 4 && refs[0] == refs[refs.len() - 1];

                    if !is_line && !is_area && !wanted.contains(&id) {
                        buf.truncate(start);
                        continue;
                    }
                    let count = buf.len() - start;
                    if is_line {
                        out.lines.push((id, line_tags(way.tags(), words)));
                    }
                    if is_area {
                        out.areas.push((id, area_tags(way.tags(), words)));
                    }
                    let chunk = (out.refs.chunks.len() - 1) as u32;
                    out.refs.index.push(Span {
                        way: id,
                        chunk,
                        start: start as u32,
                        count: count as u32,
                    });
                }
            }
        },
        |mut a, b| {
            let Ways { lines, areas, refs } = b;
            a.lines.extend(lines);
            a.areas.extend(areas);
            // `b`'s spans name `b`'s chunks, which are about to sit after
            // `a`'s, so only the chunk number shifts. The ids themselves do
            // not move: that is the point of holding them in chunks.
            let base = a.refs.chunks.len() as u32;
            a.refs.index.extend(refs.index.into_iter().map(|s| Span {
                chunk: s.chunk + base,
                ..s
            }));
            a.refs.chunks.extend(refs.chunks);
            a
        },
    )
}

/// Node coordinates, in 1e-7 degrees, for a known set of ids.
struct Locations {
    ids: Vec<i64>,
    packed: Vec<u64>,
}

impl Locations {
    /// The packed pair, which doubles as the identity of a position: ring
    /// assembly compares these rather than node ids (see `geom::assemble_rings`).
    fn get(&self, id: i64) -> Option<u64> {
        let k = self.ids.binary_search(&id).ok()?;
        Some(self.packed[k]).filter(|&v| v != MISSING)
    }
}

fn unpack(v: u64) -> Pt {
    let lat = (v >> 32) as u32 as i32;
    let lon = v as u32 as i32;
    [lon as f64 * 1e-7, lat as f64 * 1e-7]
}

/// Pass 3: coordinates, for the nodes pass 2 asked about and no others.
fn scan_nodes(blobs: &[MmapBlob<'_>], wanted: &WayRefs, pass: &str, weight: f64) -> Locations {
    // Sized exactly, then handed the slack back: on France the flat ref list is
    // ~700M ids and dedup leaves ~479M, so the unshrunk capacity would hold
    // 1.8 GB that nothing ever reads. Shrinking costs a copy at a point where
    // `packed` is not allocated yet, so it does not raise the peak.
    let mut ids: Vec<i64> = Vec::with_capacity(wanted.total());
    for chunk in &wanted.chunks {
        ids.extend_from_slice(chunk);
    }
    ids.sort_unstable();
    ids.dedup();
    ids.shrink_to_fit();

    // One slot per wanted node, written by whichever worker happens to decode
    // the block that node lives in. The slots are disjoint, so the ordering is
    // irrelevant and the stores can be relaxed.
    let packed: Vec<AtomicU64> = (0..ids.len()).map(|_| AtomicU64::new(MISSING)).collect();
    let store = |id: i64, lat: i32, lon: i32| {
        if let Ok(k) = ids.binary_search(&id) {
            let v = ((lat as u32 as u64) << 32) | lon as u32 as u64;
            packed[k].store(v, Ordering::Relaxed);
        }
    };
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let n_blobs = blobs.len().max(1);
    let each = weight / n_blobs as f64;
    blobs.par_iter().for_each(|blob| {
        if let BlobDecode::OsmData(block) = blob.decode().expect("corrupt PBF blob") {
            for group in block.groups() {
                for n in group.dense_nodes() {
                    store(n.id(), n.decimicro_lat(), n.decimicro_lon());
                }
                for n in group.nodes() {
                    store(n.id(), n.decimicro_lat(), n.decimicro_lon());
                }
            }
        }
        let i = seen.fetch_add(1, Ordering::Relaxed) + 1;
        progress::tick(each);
        if i.is_multiple_of(64) || i == n_blobs {
            progress::at(format!("{pass} {i}/{n_blobs}"));
        }
    });

    Locations {
        ids,
        packed: packed.into_iter().map(AtomicU64::into_inner).collect(),
    }
}

// --- row construction -----------------------------------------------------

fn line_tags<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>, words: &Interner) -> TagSet {
    let mut t = TagSet::default();
    for (k, v) in tags {
        match k {
            "name" => t.name = words.intern(v),
            "highway" => t.highway = words.intern(v),
            "waterway" => t.waterway = words.intern(v),
            _ => {}
        }
    }
    t
}

fn area_tags<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>, words: &Interner) -> TagSet {
    let mut t = TagSet::default();
    for (k, v) in tags {
        match k {
            "name" => t.name = words.intern(v),
            "waterway" => t.waterway = words.intern(v),
            "building" => t.building = words.intern(v),
            "landuse" => t.landuse = words.intern(v),
            "natural" => t.natural = words.intern(v),
            "leisure" => t.leisure = words.intern(v),
            "water" => t.water = words.intern(v),
            _ => {}
        }
    }
    t
}

/// Node ids to packed positions, dropping the repeats a duplicated node makes.
///
/// A missing location fails the whole way: libosmium raises
/// `InvalidLocationError` there and the old loop skipped the object, so a way
/// clipped by the extract boundary is dropped rather than short-circuited
/// across the gap.
fn resolve(ids: &[i64], locations: &Locations) -> Option<Vec<u64>> {
    let mut out: Vec<u64> = Vec::with_capacity(ids.len());
    for &id in ids {
        let p = locations.get(id)?;
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    Some(out)
}

fn build_line(id: i64, tags: Tags, ways: &WayRefs, locations: &Locations) -> Option<Row> {
    let pts: Vec<Pt> = resolve(ways.get(id)?, locations)?
        .into_iter()
        .map(unpack)
        .collect();
    if pts.len() < 2 {
        return None;
    }
    Some(Row {
        osm_id: id,
        tags,
        wkb: geom::wkb_linestring(&pts),
    })
}

/// `rings` is already assembled and resolved: closed sequences of positions.
fn build_area(osm_id: i64, tags: Tags, rings: Vec<Vec<u64>>, min_span: f64) -> Option<Row> {
    // A ring that collapses to nothing once duplicate positions are dropped is
    // just noise: discard it and keep the rest of the shape.
    let rings: Vec<Ring> = rings
        .into_iter()
        .filter(|r| r.len() >= 4)
        .map(|r| Ring::new(r.into_iter().map(unpack).collect()))
        .collect();
    let polys = geom::classify(rings);
    if polys.is_empty() {
        return None;
    }
    // Cheaper than it looks and it pays for itself many times over: this is
    // what keeps ~1.8M sub-pixel buildings per region out of the database,
    // before anything pays for WKB, DuckDB or a tile.
    if geom::outer_span(&polys) < min_span {
        return None;
    }
    Some(Row {
        osm_id,
        tags,
        wkb: geom::wkb_multipolygon(&polys),
    })
}
