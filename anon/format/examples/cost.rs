//! What a zone weighs and what a lookup costs, without needing the database.
//!
//!   cargo run --release -p anon-format --example cost
//!
//! The point is the second number. The index is delta coded in blocks, so a
//! lookup binary-searches the skip table and then scans one block -- and that
//! has to stay in the same order as a page fault, or the compression was not
//! worth having. The continent below is synthetic but sized like the real one:
//! ~4M occupied cells with gaps averaging a couple of thousand keys, which is
//! what Europe's buildings look like along the curve.

fn main() {
    let level = 18;
    let mut bins = Vec::new();
    let (mut h, mut key) = (12_345u64, 0u64);
    for _ in 0..4_000_000u64 {
        h = h
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        key += 1 + (h >> 52);
        bins.push(anon_format::Bin {
            key,
            buildings: 1 + (h >> 61) as u32,
            density: 300.0,
            built: 8.0,
        });
    }

    let zones = anon_format::cut(&bins, 64);
    let n = zones.len();
    let bounds = [-180.0, -85.0, 180.0, 85.0];
    let bytes = anon_format::encode(level, 64, bounds, &[(64, zones)]).unwrap();
    println!(
        "{n} zones, {:.1} MB, {:.2} bytes a zone",
        bytes.len() as f64 / 1e6,
        bytes.len() as f64 / n as f64
    );

    // Scattered rather than sequential, so the skip-table probes miss cache the
    // way they would under real traffic.
    let ix = anon_format::Index::parse(&bytes).unwrap();
    let reps = 200_000;
    let t = std::time::Instant::now();
    let mut acc = 0u64;
    for i in 0..reps {
        let lat = 35.0 + f64::from(i % 4_000) * 0.01;
        let lon = -10.0 + f64::from(i % 5_500) * 0.01;
        acc = acc.wrapping_add(ix.zone(&bytes, 0, lat, lon).map_or(0, |z| z.id));
    }
    let each = t.elapsed().as_nanos() as f64 / f64::from(reps);

    // Split it: rebuilding the geometry from the interval walks the quadtree once
    // per aligned square, so it is arithmetic rather than memory, and it is worth
    // knowing which half of the lookup is which.
    let starts: Vec<u64> = (0..reps)
        .map(|i| {
            let lat = 35.0 + f64::from(i % 4_000) * 0.01;
            let lon = -10.0 + f64::from(i % 5_500) * 0.01;
            ix.zone(&bytes, 0, lat, lon).map_or(0, |z| z.id)
        })
        .collect();
    let t = std::time::Instant::now();
    let mut cells = 0u64;
    for &start in &starts {
        cells += anon_format::extent_of(level, start, start + 100_000).cells;
    }
    let geometry = t.elapsed().as_nanos() as f64 / f64::from(reps);
    println!(
        "{each:.0} ns a lookup, of which ~{geometry:.0} ns rebuilding the geometry\
         \n  (checksums {acc:x} {cells:x})"
    );
}
