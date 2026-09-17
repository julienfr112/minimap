//! Minimal read-only PMTiles v3 reader.
//!
//! Only what serving needs: map z/x/y to a byte range and hand back the slice.
//! Tiles come out still gzipped, exactly as stored, so the server never
//! compresses or decompresses anything -- it just sets Content-Encoding.
//!
//! The archive is mmapped, so this holds no tile data in memory: the kernel
//! pages in the 4 kB blocks actually touched and evicts them under pressure.
//! A 14 GB archive works on a 1 GB machine. Only the root directory is parsed
//! eagerly (the spec caps it at 16 kB); leaf directories are decompressed on
//! demand and kept in a bounded cache -- see [`LEAF_CACHE_BYTES`].
//!
//! Nothing here panics on a malformed archive. A corrupt header is an error at
//! [`Archive::open`]; a corrupt leaf, which is only seen once a request steers
//! a lookup into it, reads as "no such tile".

use std::{collections::HashMap, fs::File, io::Read, path::Path, sync::Mutex};

use memmap2::Mmap;

/// How much decoded leaf directory one archive may hold, in bytes.
///
/// A leaf is gunzipped and parsed on first touch, and a hit is then two binary
/// searches instead of a gunzip -- so leaves are worth caching. But "one parsed
/// directory per leaf" is not a small number on a real archive: Europe's
/// `roads.pmtiles` has 85M tile entries in 2,917 leaves, which decoded is 2 GB,
/// and `buildings` and `water` add another 1.3 GB. Left unbounded, a client
/// that walks the map -- a crawler, a wide pan -- would grow the process past
/// the working set the box was sized for.
///
/// So the cache has a budget and drops the least recently used leaf past it.
/// 128 MB is ~5.5M entries: on `roads`, whose leaves run to 29k entries, some
/// 180 of them. A leaf at z17 spans ~50 km, so a city is one or two leaves per
/// rung and the working set of every European capital at once still fits.
/// Six archives put the ceiling at 768 MB, and only the three deep layers can
/// get near it. `make perf` checks the bound holds and measures what an
/// eviction costs: the gunzip it saved, ~10 us on the fixture's small leaves
/// and ~0.6 ms on a real archive's -- less than the disk fault a cold tile pays
/// anyway, and paid only by a client walking the continent.
pub const LEAF_CACHE_BYTES: usize = 128 << 20;

#[derive(Debug, Clone, Copy)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

/// One decoded leaf directory, and when it was last read.
struct Leaf {
    entries: Vec<Entry>,
    used: u64,
}

/// The leaf cache: decoded directories keyed by their offset in the leaf
/// section, bounded by `budget` bytes of entries.
struct Leaves {
    by_offset: HashMap<u64, Leaf>,
    bytes: usize,
    budget: usize,
    /// A logical clock: every lookup stamps the leaf it used, and eviction
    /// takes the smallest stamp. An O(n) scan over at most a few hundred
    /// leaves, and only on the miss path.
    clock: u64,
}

impl Leaves {
    fn new(budget: usize) -> Leaves {
        Leaves {
            by_offset: HashMap::new(),
            bytes: 0,
            budget,
            clock: 0,
        }
    }

    fn insert(&mut self, key: u64, entries: Vec<Entry>) {
        let size = entries.len() * std::mem::size_of::<Entry>();
        // Replacing a leaf rather than adding one: drop the old accounting
        // first, or `bytes` drifts upwards and the budget shrinks silently.
        if let Some(old) = self.by_offset.remove(&key) {
            self.bytes -= old.entries.len() * std::mem::size_of::<Entry>();
        }
        // Make room first, so a leaf larger than the whole budget still lands
        // (and is the only thing held) rather than being refused forever.
        while self.bytes + size > self.budget && !self.by_offset.is_empty() {
            let oldest = *self
                .by_offset
                .iter()
                .min_by_key(|(_, l)| l.used)
                .map(|(k, _)| k)
                .expect("non-empty");
            if let Some(gone) = self.by_offset.remove(&oldest) {
                self.bytes -= gone.entries.len() * std::mem::size_of::<Entry>();
            }
        }
        self.bytes += size;
        self.by_offset.insert(
            key,
            Leaf {
                entries,
                used: self.clock,
            },
        );
    }
}

pub struct Archive {
    map: Mmap,
    root: Vec<Entry>,
    leaf_offset: u64,
    tile_data_offset: u64,
    leaves: Mutex<Leaves>,
    pub metadata: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub center_zoom: u8,
    /// Distinct tiles addressable, from the header.
    pub tile_count: u64,
    /// Directory entries across root and leaves, from the header -- what an
    /// unbounded leaf cache would hold, at [`std::mem::size_of::<Entry>`]
    /// bytes each.
    pub entries: u64,
}

fn u64_at(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().expect("8 bytes"))
}

fn i32_at(b: &[u8], p: usize) -> i32 {
    i32::from_le_bytes(b[p..p + 4].try_into().expect("4 bytes"))
}

/// Reads one LEB128 varint, advancing `p`. `None` past the end of `b`.
fn varint(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*p)?;
        *p += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

/// Directory layout: count, then four columnar runs of varints (delta tile ids,
/// run lengths, lengths, offsets). An offset of 0 on a non-first entry means
/// "contiguous with the previous entry"; otherwise the stored value is offset+1.
///
/// `None` for anything truncated or self-contradictory.
fn parse_directory(buf: &[u8]) -> Option<Vec<Entry>> {
    let mut p = 0usize;
    let n = usize::try_from(varint(buf, &mut p)?).ok()?;
    // Four varints per entry at one byte minimum: a count the buffer cannot
    // possibly hold is refused before it is allocated for.
    if n > buf.len() {
        return None;
    }
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0
        };
        n
    ];
    let mut last = 0u64;
    for e in entries.iter_mut() {
        last = last.checked_add(varint(buf, &mut p)?)?;
        e.tile_id = last;
    }
    for e in entries.iter_mut() {
        e.run_length = u32::try_from(varint(buf, &mut p)?).ok()?;
    }
    for e in entries.iter_mut() {
        e.length = u32::try_from(varint(buf, &mut p)?).ok()?;
    }
    for i in 0..n {
        let raw = varint(buf, &mut p)?;
        entries[i].offset = if i > 0 && raw == 0 {
            entries[i - 1].offset + u64::from(entries[i - 1].length)
        } else {
            raw.checked_sub(1)?
        };
    }
    Some(entries)
}

/// Largest entry with `tile_id <= target`. Entries are sorted by id, which the
/// format guarantees and `parse_directory`'s delta coding enforces.
fn find(entries: &[Entry], target: u64) -> Option<Entry> {
    let i = entries.partition_point(|e| e.tile_id <= target);
    i.checked_sub(1).map(|i| entries[i])
}

/// z/x/y to the archive's Hilbert-curve tile id. Must match the spec exactly or
/// every lookup silently returns the wrong tile; the tests below pin it to the
/// spec's worked values.
pub fn tile_id(z: u8, x: u32, y: u32) -> Option<u64> {
    if z > 31 || x >= 1 << z || y >= 1 << z {
        return None;
    }
    let mut acc = ((1u64 << (u32::from(z) * 2)) - 1) / 3;
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    for a in (0..z).rev() {
        let s = 1u64 << a;
        let rx = s & x;
        let ry = s & y;
        acc += ((3 * rx) ^ ry) << a;
        // rotate
        if ry == 0 {
            if rx != 0 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
    }
    Some(acc)
}

/// A byte range of the header, or a readable error rather than a panic.
fn section<'a>(map: &'a Mmap, offset: u64, length: u64, what: &str) -> Result<&'a [u8], String> {
    let start = usize::try_from(offset).ok();
    let end = start.and_then(|s| usize::try_from(length).ok().and_then(|l| s.checked_add(l)));
    match (start, end) {
        (Some(s), Some(e)) if e <= map.len() => Ok(&map[s..e]),
        _ => Err(format!("{what} at {offset}+{length} lies outside the file")),
    }
}

impl Archive {
    /// Open with the default leaf-cache budget, [`LEAF_CACHE_BYTES`].
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        Self::open_with_cache(path, LEAF_CACHE_BYTES)
    }

    /// Open with an explicit leaf-cache budget in bytes. The server uses the
    /// default; this exists so `make perf` can watch a small cache evict.
    pub fn open_with_cache(
        path: &Path,
        leaf_cache_bytes: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        // SAFETY: the archive is immutable for the lifetime of the process --
        // updates ship a new file and restart rather than mutating in place.
        let map = unsafe { Mmap::map(&file)? };
        let name = path.display();
        if map.len() < 127 || &map[0..7] != b"PMTiles" {
            return Err(format!("{name} is not a PMTiles archive").into());
        }
        if map[7] != 3 {
            return Err(format!("{name}: PMTiles spec version {} unsupported", map[7]).into());
        }
        let internal_compression = map[97];
        let tile_compression = map[98];
        if internal_compression != 2 || tile_compression != 2 {
            return Err(
                format!("{name}: expected gzip for both internal and tile compression").into(),
            );
        }

        let root_raw = section(&map, u64_at(&map, 8), u64_at(&map, 16), "root directory")
            .map_err(|e| format!("{name}: {e}"))?;
        let root = parse_directory(&gunzip(root_raw)?)
            .ok_or_else(|| format!("{name}: root directory is malformed"))?;
        let meta_raw = section(&map, u64_at(&map, 24), u64_at(&map, 32), "metadata")
            .map_err(|e| format!("{name}: {e}"))?;
        let metadata = String::from_utf8(gunzip(meta_raw)?)?;

        Ok(Self {
            leaf_offset: u64_at(&map, 40),
            tile_data_offset: u64_at(&map, 56),
            tile_count: u64_at(&map, 72),
            entries: u64_at(&map, 80),
            min_zoom: map[100],
            max_zoom: map[101],
            min_lon: f64::from(i32_at(&map, 102)) / 1e7,
            min_lat: f64::from(i32_at(&map, 106)) / 1e7,
            max_lon: f64::from(i32_at(&map, 110)) / 1e7,
            max_lat: f64::from(i32_at(&map, 114)) / 1e7,
            center_zoom: map[118],
            root,
            leaves: Mutex::new(Leaves::new(leaf_cache_bytes)),
            metadata,
            map,
        })
    }

    /// How many leaf directories the root points at.
    pub fn leaf_count(&self) -> usize {
        self.root.iter().filter(|e| e.run_length == 0).count()
    }

    /// Bytes of decoded leaf directory currently held. Never exceeds the
    /// budget the archive was opened with, except when one leaf alone does.
    pub fn cached_leaf_bytes(&self) -> usize {
        self.leaves.lock().map(|l| l.bytes).unwrap_or(0)
    }

    /// The tile's bytes, still gzipped. None when the archive has no such tile.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Option<&[u8]> {
        let id = tile_id(z, x, y)?;
        let mut entry = find(&self.root, id)?;

        // run_length == 0 marks a pointer into the leaf directory section.
        if entry.run_length == 0 {
            let key = entry.offset;
            let mut cache = self.leaves.lock().ok()?;
            cache.clock += 1;
            if !cache.by_offset.contains_key(&key) {
                let start = usize::try_from(self.leaf_offset.checked_add(entry.offset)?).ok()?;
                let raw = self
                    .map
                    .get(start..start.checked_add(entry.length as usize)?)?;
                let parsed = parse_directory(&gunzip(raw).ok()?)?;
                cache.insert(key, parsed);
            }
            let clock = cache.clock;
            let leaf = cache.by_offset.get_mut(&key)?;
            leaf.used = clock;
            entry = find(&leaf.entries, id)?;
            if entry.run_length == 0 {
                return None; // no nesting deeper than one leaf level
            }
        }

        // A run covers run_length consecutive ids sharing one blob.
        if id >= entry.tile_id + u64::from(entry.run_length) {
            return None;
        }
        let start = usize::try_from(self.tile_data_offset.checked_add(entry.offset)?).ok()?;
        self.map
            .get(start..start.checked_add(entry.length as usize)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spec's own worked example: ids count tiles in Hilbert order, rung
    /// by rung, so z0 is 0, z1 is 1..4 in curve order, and z2 starts at 5.
    #[test]
    fn tile_ids_match_the_spec() {
        assert_eq!(tile_id(0, 0, 0), Some(0));
        assert_eq!(tile_id(1, 0, 0), Some(1));
        assert_eq!(tile_id(1, 0, 1), Some(2));
        assert_eq!(tile_id(1, 1, 1), Some(3));
        assert_eq!(tile_id(1, 1, 0), Some(4));
        assert_eq!(tile_id(2, 0, 0), Some(5));
        // The first id of each rung is the number of tiles above it.
        for z in 0..12u8 {
            assert_eq!(tile_id(z, 0, 0), Some(((1u64 << (2 * z)) - 1) / 3));
        }
        assert_eq!(tile_id(3, 8, 0), None, "x past the rung's edge");
        assert_eq!(tile_id(32, 0, 0), None, "z past the format's depth");
    }

    /// Every tile of a rung gets its own id, ids fill the rung's range with no
    /// gaps, and consecutive ids are adjacent tiles -- the property the whole
    /// "tiles of one viewport sit together on disk" argument rests on.
    #[test]
    fn tile_ids_walk_a_hilbert_curve() {
        for z in 1..=5u8 {
            let n = 1u32 << z;
            let first = tile_id(z, 0, 0).unwrap();
            let mut at = vec![None; (n * n) as usize];
            for x in 0..n {
                for y in 0..n {
                    let slot = (tile_id(z, x, y).unwrap() - first) as usize;
                    assert!(at[slot].is_none(), "z{z}: id {slot} used twice");
                    at[slot] = Some((x, y));
                }
            }
            for pair in at.windows(2) {
                let (a, b) = (pair[0].unwrap(), pair[1].unwrap());
                assert_eq!(
                    a.0.abs_diff(b.0) + a.1.abs_diff(b.1),
                    1,
                    "z{z}: {a:?} -> {b:?}"
                );
            }
        }
    }

    fn put(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// A directory as the writer lays it out: count, then the four columns.
    /// `explicit` writes every offset; otherwise contiguous entries use the
    /// zero shorthand the spec allows.
    fn serialise(entries: &[Entry], explicit: bool) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, entries.len() as u64);
        let mut last = 0;
        for e in entries {
            put(&mut out, e.tile_id - last);
            last = e.tile_id;
        }
        for e in entries {
            put(&mut out, u64::from(e.run_length));
        }
        for e in entries {
            put(&mut out, u64::from(e.length));
        }
        for (i, e) in entries.iter().enumerate() {
            let contiguous =
                i > 0 && entries[i - 1].offset + u64::from(entries[i - 1].length) == e.offset;
            put(
                &mut out,
                if !explicit && contiguous {
                    0
                } else {
                    e.offset + 1
                },
            );
        }
        out
    }

    fn sample() -> Vec<Entry> {
        let mut offset = 0;
        (0..50u64)
            .map(|i| {
                let e = Entry {
                    tile_id: i * 3 + 7,
                    offset,
                    length: 100 + i as u32,
                    run_length: 1 + (i % 2) as u32,
                };
                offset += u64::from(e.length);
                e
            })
            .collect()
    }

    #[test]
    fn directories_round_trip_in_both_offset_spellings() {
        let want = sample();
        for explicit in [true, false] {
            let got = parse_directory(&serialise(&want, explicit)).expect("well-formed");
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(
                    (g.tile_id, g.offset, g.length, g.run_length),
                    (w.tile_id, w.offset, w.length, w.run_length)
                );
            }
        }
    }

    #[test]
    fn malformed_directories_are_refused_not_panicked_on() {
        let good = serialise(&sample(), true);
        assert!(parse_directory(&[]).is_none(), "empty");
        for cut in [1, good.len() / 3, good.len() - 1] {
            assert!(
                parse_directory(&good[..cut]).is_none(),
                "truncated at {cut}"
            );
        }
        // A count far beyond what the bytes could hold.
        let mut huge = Vec::new();
        put(&mut huge, u64::MAX);
        assert!(parse_directory(&huge).is_none());
        // An explicit offset of 0 on the first entry has no "previous" to be
        // contiguous with and no offset-1 to decode to.
        let mut zero = Vec::new();
        put(&mut zero, 1);
        put(&mut zero, 5);
        put(&mut zero, 1);
        put(&mut zero, 10);
        put(&mut zero, 0);
        assert!(parse_directory(&zero).is_none());
        // A varint that never terminates.
        let mut p = 0;
        assert_eq!(varint(&[0xff; 12], &mut p), None);
    }

    #[test]
    fn find_takes_the_last_entry_at_or_below() {
        let entries = sample(); // ids 7, 10, 13, ...
        assert!(find(&entries, 6).is_none(), "below the first id");
        assert_eq!(find(&entries, 7).unwrap().tile_id, 7);
        assert_eq!(find(&entries, 9).unwrap().tile_id, 7, "inside a gap");
        assert_eq!(find(&entries, 10).unwrap().tile_id, 10);
        assert_eq!(
            find(&entries, 10_000).unwrap().tile_id,
            7 + 49 * 3,
            "past the end"
        );
        assert!(find(&[], 0).is_none());
    }

    #[test]
    fn leaf_cache_evicts_least_recently_used_within_budget() {
        let leaf = |n: usize| vec![sample()[0]; n];
        let size = std::mem::size_of::<Entry>();
        let mut cache = Leaves::new(10 * size);
        for key in 0..3u64 {
            cache.clock += 1;
            cache.insert(key, leaf(4));
        }
        // 12 entries asked for, 10 allowed: the first leaf went.
        assert_eq!(cache.bytes, 8 * size);
        assert!(!cache.by_offset.contains_key(&0));
        // Touch leaf 1, then insert: leaf 2 is now the oldest and goes.
        cache.clock += 1;
        cache.by_offset.get_mut(&1).unwrap().used = cache.clock;
        cache.clock += 1;
        cache.insert(3, leaf(4));
        assert!(cache.by_offset.contains_key(&1));
        assert!(!cache.by_offset.contains_key(&2));
        // A leaf larger than the whole budget is held alone rather than refused.
        cache.clock += 1;
        cache.insert(4, leaf(50));
        assert_eq!(cache.by_offset.len(), 1);
        assert_eq!(cache.bytes, 50 * size);
        // Re-inserting a key replaces it: the old size stops being counted,
        // so the budget cannot drift out from under the eviction loop.
        cache.clock += 1;
        cache.insert(4, leaf(2));
        assert_eq!(cache.by_offset.len(), 1);
        assert_eq!(cache.bytes, 2 * size);
    }
}
