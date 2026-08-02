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
//! demand and are tiny.

use std::{collections::HashMap, fs::File, io::Read, path::Path, sync::Mutex};

use memmap2::Mmap;

#[derive(Debug, Clone, Copy)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    run_length: u32,
}

pub struct Archive {
    map: Mmap,
    root: Vec<Entry>,
    leaf_offset: u64,
    tile_data_offset: u64,
    /// Leaf directories are small and reused across requests, so decoding them
    /// once is worth a lock. Bounded by the number of leaves, not by tile count.
    leaves: Mutex<HashMap<u64, Vec<Entry>>>,
    pub metadata: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub center_zoom: u8,
    pub tile_count: u64,
}

fn u64_at(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

fn i32_at(b: &[u8], p: usize) -> i32 {
    i32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

/// Reads one LEB128 varint, advancing `p`.
fn varint(b: &[u8], p: &mut usize) -> u64 {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[*p];
        *p += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
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
fn parse_directory(buf: &[u8]) -> Vec<Entry> {
    let mut p = 0usize;
    let n = varint(buf, &mut p) as usize;
    let mut entries = vec![
        Entry { tile_id: 0, offset: 0, length: 0, run_length: 0 };
        n
    ];
    let mut last = 0u64;
    for e in entries.iter_mut() {
        last += varint(buf, &mut p);
        e.tile_id = last;
    }
    for e in entries.iter_mut() {
        e.run_length = varint(buf, &mut p) as u32;
    }
    for e in entries.iter_mut() {
        e.length = varint(buf, &mut p) as u32;
    }
    for i in 0..n {
        let raw = varint(buf, &mut p);
        entries[i].offset = if i > 0 && raw == 0 {
            entries[i - 1].offset + u64::from(entries[i - 1].length)
        } else {
            raw - 1
        };
    }
    entries
}

/// Largest entry with tile_id <= target.
fn find(entries: &[Entry], target: u64) -> Option<Entry> {
    let mut lo = 0i64;
    let mut hi = entries.len() as i64 - 1;
    let mut found = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let e = entries[mid as usize];
        if e.tile_id <= target {
            found = Some(e);
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    found
}

/// z/x/y to the archive's Hilbert-curve tile id. Must match the spec exactly or
/// every lookup silently returns the wrong tile.
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

impl Archive {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(path)?;
        // SAFETY: the archive is immutable for the lifetime of the process --
        // updates ship a new file and restart rather than mutating in place.
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < 127 || &map[0..7] != b"PMTiles" {
            return Err(format!("{} is not a PMTiles archive", path.display()).into());
        }
        if map[7] != 3 {
            return Err(format!("PMTiles spec version {} unsupported", map[7]).into());
        }
        let root_offset = u64_at(&map, 8) as usize;
        let root_length = u64_at(&map, 16) as usize;
        let metadata_offset = u64_at(&map, 24) as usize;
        let metadata_length = u64_at(&map, 32) as usize;
        let internal_compression = map[97];
        let tile_compression = map[98];
        if internal_compression != 2 || tile_compression != 2 {
            return Err("expected gzip for both internal and tile compression".into());
        }

        let root = parse_directory(&gunzip(&map[root_offset..root_offset + root_length])?);
        let metadata =
            String::from_utf8(gunzip(&map[metadata_offset..metadata_offset + metadata_length])?)?;

        Ok(Self {
            leaf_offset: u64_at(&map, 40),
            tile_data_offset: u64_at(&map, 56),
            tile_count: u64_at(&map, 72),
            min_zoom: map[100],
            max_zoom: map[101],
            min_lon: f64::from(i32_at(&map, 102)) / 1e7,
            min_lat: f64::from(i32_at(&map, 106)) / 1e7,
            max_lon: f64::from(i32_at(&map, 110)) / 1e7,
            max_lat: f64::from(i32_at(&map, 114)) / 1e7,
            center_zoom: map[118],
            root,
            leaves: Mutex::new(HashMap::new()),
            metadata,
            map,
        })
    }

    /// The tile's bytes, still gzipped. None when the archive has no such tile.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Option<&[u8]> {
        let id = tile_id(z, x, y)?;
        let mut entry = find(&self.root, id)?;

        // run_length == 0 marks a pointer into the leaf directory section.
        if entry.run_length == 0 {
            let key = entry.offset;
            let mut cache = self.leaves.lock().unwrap();
            let leaf = match cache.get(&key) {
                Some(leaf) => leaf,
                None => {
                    let start = (self.leaf_offset + entry.offset) as usize;
                    let raw = self.map.get(start..start + entry.length as usize)?;
                    let parsed = parse_directory(&gunzip(raw).ok()?);
                    cache.entry(key).or_insert(parsed)
                }
            };
            entry = find(leaf, id)?;
            if entry.run_length == 0 {
                return None; // no nesting deeper than one leaf level
            }
        }

        // A run covers run_length consecutive ids sharing one blob.
        if id >= entry.tile_id + u64::from(entry.run_length) {
            return None;
        }
        let start = (self.tile_data_offset + entry.offset) as usize;
        self.map.get(start..start + entry.length as usize)
    }
}
