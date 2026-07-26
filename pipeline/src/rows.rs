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
