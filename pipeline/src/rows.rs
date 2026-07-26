//! Extracted rows, and the way they get into DuckDB.
//!
//! Bulk loading must not go through `INSERT`. An early version of this pipeline
//! inserted staged rows one statement at a time and spent over 40 minutes
//! without finishing a single region: DuckDB is columnar, so each row paid the
//! whole statement pipeline plus a WAL write.
//!
//! The appender is the intended bulk path and is a different thing entirely. It
//! writes values straight into a DuckDB vector and hands the database a whole
//! column chunk at a time; `append_row` names a row but does not cost one.
//!
//! The columns are exactly `config::RAW_DDL`, in order. A row carries the tags
//! its kind can use and nulls elsewhere: a line never carries `building`, an
//! area never carries `highway`.

use std::collections::HashMap;
use std::sync::RwLock;

use duckdb::Appender;

/// Rows per batch. Bounds memory regardless of extract size, and is the unit
/// the geometry building is parallelised over.
pub const BATCH: usize = 100_000;

/// Every tag the classification SQL looks at. Anything not in this list is
/// dropped during extraction rather than carried through the whole pipeline.
#[derive(Default)]
pub struct Tags {
    pub name: Option<String>,
    pub highway: Option<String>,
    pub waterway: Option<String>,
    pub building: Option<String>,
    pub landuse: Option<String>,
    pub natural: Option<String>,
    pub leisure: Option<String>,
    pub water: Option<String>,
}

pub struct Row {
    pub osm_id: i64,
    pub tags: Tags,
    pub wkb: Vec<u8>,
}

/// An absent tag.
pub const NO_TAG: u32 = u32::MAX;

/// [`Tags`] as ids into an [`Interner`], which is how the extractor holds them
/// between pass 2 and emission.
///
/// This is the extractor's largest structure by a wide margin -- one per
/// candidate object, and Belgium has 10.5M of them -- so the representation is
/// worth caring about. Eight `Option<String>` is 192 bytes plus a heap
/// allocation per present tag, to store values drawn from a vocabulary of a few
/// hundred: `building=yes` said six million times. As ids it is 32 bytes flat,
/// and each distinct value is allocated once for the whole extract.
///
/// `name` is interned too. It is the one high-cardinality field, so it wins
/// nothing on allocation count, but keeping the set uniform costs nothing
/// either -- a name is stored once whether or not it has an id.
#[derive(Clone, Copy)]
pub struct TagSet {
    pub name: u32,
    pub highway: u32,
    pub waterway: u32,
    pub building: u32,
    pub landuse: u32,
    pub natural: u32,
    pub leisure: u32,
    pub water: u32,
}

impl Default for TagSet {
    fn default() -> TagSet {
        TagSet {
            name: NO_TAG,
            highway: NO_TAG,
            waterway: NO_TAG,
            building: NO_TAG,
            landuse: NO_TAG,
            natural: NO_TAG,
            leisure: NO_TAG,
            water: NO_TAG,
        }
    }
}

impl TagSet {
    /// Back to owned strings, at emission. These allocations are transient --
    /// one `BATCH` of rows at a time -- where the `TagSet`s they come from are
    /// held for the whole extract.
    pub fn resolve(&self, words: &Interner) -> Tags {
        Tags {
            name: words.get(self.name),
            highway: words.get(self.highway),
            waterway: words.get(self.waterway),
            building: words.get(self.building),
            landuse: words.get(self.landuse),
            natural: words.get(self.natural),
            leisure: words.get(self.leisure),
            water: words.get(self.water),
        }
    }
}

#[derive(Default)]
struct Table {
    ids: HashMap<Box<str>, u32>,
    values: Vec<Box<str>>,
}

/// The distinct tag values of one extract, shared by every worker in pass 2.
#[derive(Default)]
pub struct Interner {
    table: RwLock<Table>,
}

impl Interner {
    /// The id of `value`, adding it the first time it is seen.
    ///
    /// Overwhelmingly read-mostly: the vocabulary is saturated within the first
    /// few thousand objects, so tens of millions of lookups take the read lock
    /// and a few hundred take the write lock.
    pub fn intern(&self, value: &str) -> u32 {
        if let Some(&id) = self.table.read().expect("interner").ids.get(value) {
            return id;
        }
        let mut table = self.table.write().expect("interner");
        // Another worker may have inserted it between the two locks.
        if let Some(&id) = table.ids.get(value) {
            return id;
        }
        let id = u32::try_from(table.values.len()).expect("tag vocabulary fits in u32");
        assert!(id != NO_TAG, "tag vocabulary exhausted");
        let value: Box<str> = value.into();
        table.values.push(value.clone());
        table.ids.insert(value, id);
        id
    }

    /// `None` for [`NO_TAG`], the value otherwise.
    pub fn get(&self, id: u32) -> Option<String> {
        if id == NO_TAG {
            return None;
        }
        Some(self.table.read().expect("interner").values[id as usize].to_string())
    }
}

/// Where extracted rows go. A trait so the extractor does not need to know
/// whether it is feeding a database or a test.
pub trait Sink {
    fn write(&mut self, kind: &str, rows: &[Row]) -> Result<(), Box<dyn std::error::Error>>;
}

pub struct RawTable<'a> {
    appender: Appender<'a>,
    pub written: u64,
}

impl<'a> RawTable<'a> {
    pub fn new(appender: Appender<'a>) -> RawTable<'a> {
        RawTable { appender, written: 0 }
    }

    /// Flushes whatever is still buffered. Dropping the appender instead would
    /// lose the tail of the last region without saying so.
    pub fn finish(mut self) -> Result<u64, Box<dyn std::error::Error>> {
        self.appender.flush()?;
        Ok(self.written)
    }
}

impl Sink for RawTable<'_> {
    fn write(&mut self, kind: &str, rows: &[Row]) -> Result<(), Box<dyn std::error::Error>> {
        for row in rows {
            let t = &row.tags;
            self.appender.append_row((
                kind,
                row.osm_id,
                &t.name,
                &t.highway,
                &t.waterway,
                &t.building,
                &t.landuse,
                &t.natural,
                &t.leisure,
                &t.water,
                &row.wkb,
            ))?;
        }
        self.written += rows.len() as u64;
        Ok(())
    }
}
