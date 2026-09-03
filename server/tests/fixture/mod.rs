//! A valid PMTiles v3 archive, written here rather than baked, so the server's
//! tests need nothing from the pipeline.
//!
//! The tests are about directories and lookups, not about map data, so the
//! tiles hold deterministic filler. What has to be real is the structure:
//! gzipped root and leaves, one leaf level, entries in Hilbert order -- because
//! that structure is what the reader parses and the cache caches.

// Each test binary compiles its own copy of this module and uses a different
// subset of it.
#![allow(dead_code)]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use minimap_server::pmtiles::{tile_id, Archive};

/// Entries per leaf directory. Real archives pick this to keep the root under
/// the spec's 16 kB; here it is small on purpose, so a modest fixture still has
/// hundreds of leaves to miss on.
pub const LEAF_SIZE: usize = 128;

pub struct Fixture {
    pub path: PathBuf,
    /// Every z/x/y in the archive, in id order. Tests sample this rather than
    /// guessing coordinates.
    pub tiles: Vec<(u8, u32, u32)>,
    pub leaves: usize,
    pub bytes: u64,
}

impl Fixture {
    /// Every tile of every rung `0..=max_zoom`, with bodies derived from
    /// `salt` -- so two fixtures with different salts differ in every byte and
    /// in total size, which is what a re-bake looks like to an etag.
    pub fn write(path: &Path, max_zoom: u8, salt: u64) -> Fixture {
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
            (0..=max_zoom)
                .map(|z| z.to_string())
                .collect::<Vec<_>>()
                .join(",")
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
        fs::create_dir_all(path.parent().unwrap()).unwrap();
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

    /// The gzipped bytes the archive holds for one tile, recomputed -- what a
    /// request for it must come back with, byte for byte.
    pub fn stored(&self, z: u8, x: u32, y: u32, salt: u64) -> Vec<u8> {
        gzip(&filler(tile_id(z, x, y).unwrap() ^ salt, z))
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

/// xorshift64*, so every run samples the same tiles in the same order. A perf
/// number that moves because the sample moved is not a perf number.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}
