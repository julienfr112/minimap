//! What *this run* is doing: where things go, and how deep to go.
//!
//! One directory per kind of thing, so what a directory holds is its name:
//!
//!   * `pbf/` was downloaded. Expensive to fetch, polite to fetch slowly, and
//!     byte-identical on every rebuild, so nothing ever deletes it.
//!   * `duckdb/` is the database built from it. Enormous — 154 GB for Europe at
//!     z14 — and pure scaffolding once the archives exist.
//!   * `pmtiles/` is the deliverable: one archive per layer, and the only thing
//!     the server needs.
//!
//! Each is a separate flag, which matters because they differ by three orders of
//! magnitude: `--duckdb /mnt/big/duckdb` puts the database where there is room
//! without moving the 135 MB of archives off the machine that serves them.
//!
//! Note what is *not* here. The zoom rungs and the size thresholds are in
//! [`crate::tuning`] as constants, not flags: they are what this map is rather
//! than where this run puts it, and one configuration needs no machinery to
//! select between configurations.
//!
//! There used to be one hardcoded root taken from `CARGO_MANIFEST_DIR`, with
//! artefacts landing beside the source. That is why the repository grew a
//! 153 GB database, a 15 GB archive and seven stray `.log` files that nothing
//! knew how to remove.

use std::path::{Path, PathBuf};

use duckdb::Connection;


type Error = Box<dyn std::error::Error>;

/// One extract on disk: the name it is known by, and where it actually is.
pub struct Region {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The extracts, and the coastline that comes with them. Never written to
    /// by anything but `download`.
    pub pbf: PathBuf,
    /// The build database and its spill.
    pub duckdb: PathBuf,
    /// One archive per layer: the deliverable.
    pub pmtiles: PathBuf,
    /// DuckDB's budget, as DuckDB spells sizes ('8GB'). `None` means half of
    /// this machine's RAM; see [`Config::connect`].
    pub memory: Option<String>,
    /// Concurrent downloads.
    pub jobs: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            pbf: PathBuf::from("pbf"),
            duckdb: PathBuf::from("duckdb"),
            pmtiles: PathBuf::from("pmtiles"),
            memory: None,
            jobs: 3,
        }
    }
}

impl Config {
    // --- paths -------------------------------------------------------------

    /// The coastline dataset. It lives with the extracts because it shares
    /// their lifecycle -- downloaded once, never rebuilt, never cleaned -- even
    /// though it is a shapefile and not a PBF.
    pub fn land_zip(&self) -> PathBuf {
        self.pbf.join("land-polygons-split-3857.zip")
    }

    /// GDAL reads shapefiles straight out of a zip, so the 1.3 GB inside that
    /// 948 MB archive never has to be unpacked. The polygons are already
    /// EPSG:3857 -- unlike the PBFs, nothing here needs reprojecting.
    pub fn land_shp(&self) -> String {
        format!(
            "/vsizip/{}/land-polygons-split-3857/land_polygons.shp",
            self.land_zip().display()
        )
    }

    /// Geofabrik's catalogue, cached. A dotfile so `ls pbf/` shows only data.
    pub fn index_json(&self) -> PathBuf {
        self.pbf.join(".geofabrik-index.json")
    }

    pub fn db(&self) -> PathBuf {
        self.duckdb.join("minimap.duckdb")
    }

    /// One archive per layer. A directory rather than a file, because which
    /// layers exist is a property of the build.
    pub fn tiles_dir(&self) -> PathBuf {
        self.pmtiles.clone()
    }

    pub fn layer_archive(&self, layer: &str) -> PathBuf {
        self.pmtiles.join(format!("{layer}.pmtiles"))
    }

    /// Create what this run will write into. `data/` too: `download` is the
    /// first thing anyone runs.
    pub fn prepare(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.pbf)?;
        std::fs::create_dir_all(&self.duckdb)?;
        std::fs::create_dir_all(&self.pmtiles)?;
        Ok(())
    }

    // --- extracts ----------------------------------------------------------

    /// Every `.osm.pbf` under `data/`, by the name it is known by.
    ///
    /// Searched recursively and by suffix rather than by consulting a table of
    /// known regions, so any extract dropped in by hand is loadable and the
    /// older `data/countries/` layout keeps working untouched. Geofabrik's own
    /// `-latest` is stripped, so `france-latest.osm.pbf` and `france.osm.pbf`
    /// are the same region under either name.
    pub fn extracts(&self) -> Vec<Region> {
        let mut found = Vec::new();
        collect_pbfs(&self.pbf, &mut found);
        // Two files can claim one name -- `data/countries/france-latest.osm.pbf`
        // from an older layout beside `pbf/france.osm.pbf` from this one.
        // Sorting the canonical location first makes the survivor deterministic
        // rather than whatever readdir happened to return, so a load never
        // silently picks the stale copy.
        let canonical = self.pbf.clone();
        found.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then((a.path.parent() != Some(&canonical)).cmp(&(b.path.parent() != Some(&canonical))))
        });
        found.dedup_by(|a, b| a.name == b.name);
        found
    }

    /// The extracts to work from: the ones named, or everything present.
    ///
    /// Naming one that is not downloaded is an error rather than a warning. A
    /// load that quietly skips a missing country produces a map with a hole in
    /// it and no indication of why, hours later.
    pub fn regions(&self, wanted: &[String]) -> Result<Vec<Region>, Error> {
        let present = self.extracts();
        if wanted.is_empty() {
            if present.is_empty() {
                return Err(format!(
                    "no .osm.pbf under {} -- run `make download` first",
                    self.pbf.display()
                )
                .into());
            }
            return Ok(present);
        }
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for name in wanted {
            match present.iter().position(|r| &r.name == name) {
                Some(i) => out.push(Region {
                    name: present[i].name.clone(),
                    path: present[i].path.clone(),
                }),
                None => missing.push(name.as_str()),
            }
        }
        if !missing.is_empty() {
            return Err(format!(
                "not downloaded: {} -- run `make download REGIONS=\"{}\"`",
                missing.join(", "),
                missing.join(" ")
            )
            .into());
        }
        Ok(out)
    }

    // --- the database ------------------------------------------------------

    /// The one way to open the build database, so every step gets the same
    /// spatial extension and the same memory budget.
    pub fn connect(&self, read_only: bool) -> Result<Connection, Error> {
        let path = self.db();
        if read_only && !path.exists() {
            return Err(format!(
                "{} does not exist yet -- run `make load` first",
                path.display()
            )
            .into());
        }
        if !read_only {
            std::fs::create_dir_all(&self.duckdb)?;
        }
        let con = if read_only {
            Connection::open_with_flags(
                &path,
                duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
            )?
        } else {
            Connection::open(&path)?
        };
        con.execute_batch("INSTALL spatial; LOAD spatial; SET preserve_insertion_order = false;")?;

        // DuckDB's own default is 80% of system RAM, which is the wrong budget
        // here: this process is also the extractor, and at France/z15 the two
        // together reached 33 GB on a 36 GB machine and were OOM-killed midway
        // through classifying areas. Half the machine leaves room for that, and
        // a bounded DuckDB spills to disk rather than growing -- which is the
        // behaviour we want, since disk is the resource we have most of.
        //
        // DuckDB wants an absolute size (it rejects '50%'), so read the
        // machine's and halve it. Anywhere without /proc, keep DuckDB's own
        // default.
        let limit = self.memory.clone().or_else(half_of_ram);
        if let Some(limit) = limit {
            con.execute_batch(&format!("SET memory_limit = '{limit}'"))?;
        }

        // Temp files are the largest thing the bake writes -- more than 80 GB
        // spilled at Europe/z14 -- so they belong beside the database, whose
        // disk was chosen with room for them, not in DuckDB's own default or in
        // /tmp, which here is a 19 GB tmpfs.
        let tmp = self.duckdb.join("tmp");
        con.execute_batch(&format!(
            "SET temp_directory = '{}'",
            tmp.display().to_string().replace('\'', "''")
        ))?;
        Ok(con)
    }

}

/// Half of this machine's RAM, as DuckDB spells sizes.
fn half_of_ram() -> Option<String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(format!("{}MiB", kb / 1024 / 2))
}

fn collect_pbfs(dir: &Path, out: &mut Vec<Region>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_pbfs(&path, out);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".osm.pbf") {
            out.push(Region {
                name: stem.strip_suffix("-latest").unwrap_or(stem).to_string(),
                path,
            });
        }
    }
}
