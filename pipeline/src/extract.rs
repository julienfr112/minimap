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

use crate::config::{self, MIN_SPAN};
use crate::geom::{self, Pt, Ring};
use crate::rows::{Interner, Row, Sink, TagSet, Tags, BATCH};

/// Waterways worth drawing as a line. Everything else tagged `waterway` is
/// either an area or too small to matter.
const WATERWAY_LINES: [&str; 5] = ["river", "canal", "stream", "ditch", "drain"];

/// A closed way with any of these is a candidate area.
const AREA_KEYS: [&str; 6] = ["building", "landuse", "natural", "leisure", "water", "waterway"];

/// No coordinate stored for this node yet. A real node at exactly
/// (-1e-7, -1e-7) would be indistinguishable; it is 11 metres off Null Island
/// and it does not exist.
const MISSING: u64 = u64::MAX;

pub fn run(path: &Path, sink: &mut impl Sink) -> Result<(), Box<dyn std::error::Error>> {
    let min_span = MIN_SPAN;
    // Mapping the file gives every worker a cheap view of it; the blob list is
    // just offsets and lengths into that map, so the three passes below re-read
    // the file without re-scanning its framing.
    let mmap = unsafe { Mmap::from_path(path)? };
    let blobs: Vec<MmapBlob<'_>> = mmap.blob_iter().collect::<osmpbf::Result<_>>()?;

    // One vocabulary for the whole extract, shared by both scanning passes.
    let words = Interner::default();

    let relations = scan_relations(&blobs, &words);
    let wanted: HashSet<i64> = relations.iter().flat_map(|r| r.members.iter().copied()).collect();

    let mut ways = scan_ways(&blobs, &wanted, &words);
    ways.refs.index.sort_unstable_by_key(|&(id, _, _)| id);

    let locations = scan_nodes(&blobs, &ways.refs.nodes);
    config::log(format!(
        "  {} ways kept, {} multipolygons, {} node locations",
        config::commas(ways.refs.len() as u64),
        config::commas(relations.len() as u64),
        config::commas(locations.ids.len() as u64),
    ));

    let (mut n_lines, mut n_way_areas, mut n_rel_areas) = (0, 0, 0);

    for chunk in ways.lines.chunks(BATCH) {
        let batch: Vec<Row> = chunk
            .par_iter()
            .filter_map(|(id, tags)| build_line(*id, tags.resolve(&words), &ways.refs, &locations))
            .collect();
        n_lines += batch.len();
        sink.write("line", &batch)?;
    }
    for chunk in ways.areas.chunks(BATCH) {
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
    }
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
    }

    config::log(format!(
        "  {} lines, {} areas ({} ways + {} relations)",
        config::commas(n_lines as u64),
        config::commas((n_way_areas + n_rel_areas) as u64),
        config::commas(n_way_areas as u64),
        config::commas(n_rel_areas as u64),
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
    empty: impl Fn() -> T + Sync + Send,
    visit: impl Fn(&PrimitiveBlock, &mut T) + Sync + Send,
    merge: impl Fn(T, T) -> T + Sync + Send,
) -> T {
    blobs
        .par_iter()
        .fold(&empty, |mut acc, blob| {
            if let BlobDecode::OsmData(block) = blob.decode().expect("corrupt PBF blob") {
                visit(&block, &mut acc);
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
fn scan_relations(blobs: &[MmapBlob<'_>], words: &Interner) -> Vec<RelArea> {
    scan(
        blobs,
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
                    out.push(RelArea { id: rel.id(), tags: area_tags(rel.tags(), words), members });
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

/// Node ids of many ways, CSR-style: one flat `nodes` array plus an `index` of
/// `(way id, offset, count)` sorted by way id.
///
/// The obvious `Vec<(i64, Vec<i64>)>` costs a heap allocation per way, and a
/// dense country has millions of them: Belgium's 10.5M ways paid 24 bytes of
/// `Vec` header plus a size-class-rounded allocation each, on top of the node
/// ids themselves. Here the ids live end to end in one buffer and a way is 16
/// bytes of index.
///
/// Sorting touches only the index, so node data is never permuted -- which is
/// also why `offset` can stay a `u32`: it indexes one extract's nodes, and the
/// largest country extract is two orders of magnitude below `u32::MAX`.
#[derive(Default)]
struct WayRefs {
    index: Vec<(i64, u32, u32)>,
    nodes: Vec<i64>,
}

impl WayRefs {
    fn len(&self) -> usize {
        self.index.len()
    }

    /// The node ids of one way. `None` if this extract does not contain it.
    fn get(&self, id: i64) -> Option<&[i64]> {
        let k = self.index.binary_search_by_key(&id, |&(w, _, _)| w).ok()?;
        let (_, start, count) = self.index[k];
        Some(&self.nodes[start as usize..start as usize + count as usize])
    }
}

/// Pass 2: the ways we draw, plus the ways the wanted relations are built from.
fn scan_ways(blobs: &[MmapBlob<'_>], wanted: &HashSet<i64>, words: &Interner) -> Ways {
    scan(
        blobs,
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
                    let start = out.refs.nodes.len();
                    out.refs.nodes.extend(way.refs());
                    let refs = &out.refs.nodes[start..];
                    // libosmium only builds areas from closed ways, and a ring
                    // needs three corners plus the repeated first node.
                    let is_area = area_key
                        && !area_no
                        && refs.len() >= 4
                        && refs[0] == refs[refs.len() - 1];

                    if !is_line && !is_area && !wanted.contains(&id) {
                        out.refs.nodes.truncate(start);
                        continue;
                    }
                    if is_line {
                        out.lines.push((id, line_tags(way.tags(), words)));
                    }
                    if is_area {
                        out.areas.push((id, area_tags(way.tags(), words)));
                    }
                    let count = out.refs.nodes.len() - start;
                    out.refs.index.push((id, start as u32, count as u32));
                }
            }
        },
        |mut a, b| {
            a.lines.extend(b.lines);
            a.areas.extend(b.areas);
            // Each thread's offsets are relative to its own buffer, so they
            // shift by however much is already in `a`.
            let base = a.refs.nodes.len() as u32;
            a.refs.index.reserve_exact(b.refs.index.len());
            a.refs.index.extend(b.refs.index.iter().map(|&(id, s, n)| (id, s + base, n)));
            a.refs.nodes.reserve_exact(b.refs.nodes.len());
            a.refs.nodes.extend(b.refs.nodes);
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
///
/// `wanted` is pass 2's flat node array, so this is a copy-sort-dedup of it
/// rather than a gather across millions of separate ways.
fn scan_nodes(blobs: &[MmapBlob<'_>], wanted: &[i64]) -> Locations {
    let mut ids: Vec<i64> = wanted.to_vec();
    ids.sort_unstable();
    ids.dedup();

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
    });

    Locations { ids, packed: packed.into_iter().map(AtomicU64::into_inner).collect() }
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
    let pts: Vec<Pt> = resolve(ways.get(id)?, locations)?.into_iter().map(unpack).collect();
    if pts.len() < 2 {
        return None;
    }
    Some(Row { osm_id: id, tags, wkb: geom::wkb_linestring(&pts) })
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
    Some(Row { osm_id, tags, wkb: geom::wkb_multipolygon(&polys) })
}
