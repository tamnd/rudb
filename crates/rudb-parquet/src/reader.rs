//! Reading a Parquet file as a run of chunks.
//!
//! The reader works a row group at a time, which is the unit the format stores and the unit the
//! scheduler will hand out as a morsel. For each row group it reads the projected columns, decodes
//! each one into vectors of at most `VECTOR_SIZE`, and zips them into chunks. The split is by row
//! index, so every column of a group splits in the same place and the pieces line up without
//! anybody coordinating.
//!
//! Projection is the whole reason the format exists and it is the first thing this does. A
//! ClickBench query touches two or three of a hundred and five columns, and the difference between
//! reading those and reading all of them on `hits.parquet` is the difference between two hundred
//! megabytes and twenty gigabytes. The reader never reads a column nobody asked for, and
//! [`Reader::bytes_read`] is how a test asserts that rather than inferring it from a timing.
//!
//! A projection of no columns is a real query rather than a mistake. `SELECT count(*)` wants the
//! row count and none of the data, and it comes back here as chunks that are the right length and
//! hold nothing, having read the footer and not one byte more.
//!
//! # What this does not do yet
//!
//! One read per column chunk, synchronous, through [`File::read_exact_at`]. The submission
//! interface in `rudb-io` is what a row group's worth of reads should go through as one batch, and
//! wiring it is the next piece of 2d rather than this one. Row group statistics are in the footer
//! and are not consulted, because pruning needs a predicate and there is nothing to push down yet.
//! Both are E2 in `spec/engine/05-scan.md` and both are named here so that their absence is a
//! decision rather than an oversight.

use std::collections::VecDeque;

use rudb_common::{Error, Field, Result};
use rudb_io::File;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::column;
use crate::metadata::Metadata;

/// A Parquet file, read as chunks.
#[derive(Debug)]
pub struct Reader {
    file: Box<dyn File>,
    metadata: Metadata,
    projection: Vec<usize>,
    group: usize,
    ready: VecDeque<Chunk>,
    bytes: u64,
}

impl Reader {
    /// Opens a file, reading its footer and nothing else.
    ///
    /// Every column is projected until [`Reader::project`] says otherwise.
    ///
    /// # Errors
    ///
    /// If the file is not Parquet or its footer does not parse.
    pub fn open(file: Box<dyn File>) -> Result<Self> {
        let metadata = Metadata::read(file.as_ref())?;
        let projection = (0..metadata.schema.len()).collect();
        Ok(Self { file, metadata, projection, group: 0, ready: VecDeque::new(), bytes: 0 })
    }

    /// The footer.
    #[must_use]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Reads only these columns, in this order.
    ///
    /// The order is the caller's and not the file's, so a query selecting the last column and then
    /// the first gets them that way round without a projection above the scan. A column named twice
    /// is read twice, which is what a query asking for it twice means.
    ///
    /// # Errors
    ///
    /// If a column index is not one the file has. Taking indices rather than names is deliberate:
    /// name resolution is the binder's job and it has rules about case and quoting that a file
    /// reader should not be having opinions about.
    pub fn project(&mut self, columns: &[usize]) -> Result<()> {
        for &column in columns {
            if column >= self.metadata.schema.len() {
                return Err(Error::io(format!(
                    "column {column} of a parquet file with {} columns",
                    self.metadata.schema.len()
                )));
            }
        }
        self.projection = columns.to_vec();
        self.ready.clear();
        Ok(())
    }

    /// The columns the reader produces, in the order it produces them.
    ///
    /// A column the file marked required comes back as `NOT NULL`, because the writer promised it
    /// and the reader has no reason to be vaguer than the file was.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.projection
            .iter()
            .map(|&at| {
                let column = &self.metadata.schema[at];
                if column.optional {
                    Field::new(column.name.clone(), column.ty.clone())
                } else {
                    Field::required(column.name.clone(), column.ty.clone())
                }
            })
            .collect()
    }

    /// The next chunk, or nothing when the file is done.
    ///
    /// # Errors
    ///
    /// If a read fails, a page does not decode, or the file uses a codec or an encoding this build
    /// does not have.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        while self.ready.is_empty() {
            if self.group >= self.metadata.row_groups.len() {
                return Ok(None);
            }
            let group = self.group;
            self.group += 1;
            self.read_group(group)?;
        }
        Ok(self.ready.pop_front())
    }

    /// How many bytes of column data have been read.
    ///
    /// The footer is not counted, because it is read once whatever the query is and a projection
    /// test wants the number that changes. This is what `spec/engine/05-scan.md` section 5.6
    /// requires next to every query: a pruning bug that reads too much is invisible in an answer
    /// and obvious here.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    /// Reads one row group and queues the chunks it holds.
    fn read_group(&mut self, at: usize) -> Result<()> {
        let group = &self.metadata.row_groups[at];
        let rows = usize::try_from(group.rows)
            .map_err(|_| Error::io(format!("a row group of {} rows", group.rows)))?;
        if rows == 0 {
            return Ok(());
        }
        let mut columns: Vec<Vec<Vector>> = Vec::with_capacity(self.projection.len());
        for &wanted in &self.projection {
            let chunk =
                group.columns.iter().find(|candidate| candidate.column == wanted).ok_or_else(
                    || {
                        Error::io(format!(
                            "a row group with no chunk for column {}",
                            self.metadata.schema[wanted].name
                        ))
                    },
                )?;
            let len = usize::try_from(chunk.compressed_size).map_err(|_| {
                Error::io(format!("a column chunk of {} bytes", chunk.compressed_size))
            })?;
            let mut bytes = vec![0_u8; len];
            self.file.read_exact_at(chunk.start(), &mut bytes)?;
            self.bytes += len as u64;
            columns.push(column::decode(&self.metadata.schema[wanted], chunk, &bytes, rows)?);
        }
        self.queue(columns, rows)
    }

    /// Zips the decoded columns of one row group into chunks.
    ///
    /// A projection of no columns has no vectors to take a length from, so the lengths come from
    /// the row count instead. That is the `SELECT count(*)` path and it is the only one where the
    /// chunk's length is not a property of anything in it.
    fn queue(&mut self, columns: Vec<Vec<Vector>>, rows: usize) -> Result<()> {
        let pieces = columns.first().map_or_else(|| rows.div_ceil(VECTOR_SIZE), Vec::len);
        for column in &columns {
            if column.len() != pieces {
                return Err(Error::internal(
                    "two columns of one parquet row group split into different numbers of chunks",
                ));
            }
        }
        let mut left = rows;
        let mut columns: Vec<std::vec::IntoIter<Vector>> =
            columns.into_iter().map(Vec::into_iter).collect();
        for _ in 0..pieces {
            let mut vectors = Vec::with_capacity(columns.len());
            for column in &mut columns {
                vectors.push(column.next().ok_or_else(|| {
                    Error::internal("a parquet column that ran out of chunks part way through")
                })?);
            }
            let len = vectors.first().map_or(left.min(VECTOR_SIZE), Vector::len);
            self.ready.push_back(Chunk::with_rows(vectors, len)?);
            left -= len;
        }
        Ok(())
    }
}

/// Reads a whole file into chunks, projecting every column.
///
/// The convenience a test wants and the thing a scan should not do, because it holds the file in
/// memory. It is here rather than in each test so that the loop over [`Reader::next_chunk`] is
/// written once.
///
/// # Errors
///
/// If the file does not open or does not decode.
pub fn read(file: Box<dyn File>) -> Result<Vec<Chunk>> {
    let mut reader = Reader::open(file)?;
    let mut out = Vec::new();
    while let Some(chunk) = reader.next_chunk()? {
        out.push(chunk);
    }
    Ok(out)
}
