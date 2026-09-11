//! Reading a whole Parquet file as a run of chunks.
//!
//! This is the layer above the page walker. [`Pages`] turns one column chunk's bytes into pages and
//! `Page::into_vector` turns one page into a vector, and what is left is everything that is about
//! the file rather than about a page: which columns to read, where their chunks are, and how to put
//! seven columns of a row group side by side into something the engine can execute over.
//!
//! The reader works a row group at a time, which is the unit the format stores and the unit the
//! scheduler will hand out as a morsel.
//!
//! # Projection is the point of the format
//!
//! A ClickBench query touches two or three of a hundred and five columns, and the difference
//! between reading those and reading all of them on `hits.parquet` is the difference between two
//! hundred megabytes and twenty gigabytes. The reader never reads a column nobody asked for, and
//! [`Reader::bytes_read`] is how a test asserts that rather than inferring it from a timing.
//!
//! A projection of no columns is a real query rather than a mistake. `SELECT count(*)` wants the
//! row count and none of the data, and it comes back from here as chunks that are the right length
//! and hold nothing, having read the footer and not one byte more.
//!
//! # Why the chunks come out on page boundaries
//!
//! Two columns of one row group are free to page differently. The writer decides where a page ends
//! by how many bytes it has written, so a chunk of `BIGINT` and a chunk of `BOOLEAN` covering the
//! same rows can have wildly different page counts, and nothing in the format makes them line up.
//!
//! So a chunk ends wherever the first of the projected columns runs out of its current page, or at
//! [`VECTOR_SIZE`] rows, whichever comes first. A column whose page ends exactly there hands the
//! page over whole and nothing is copied at all. Any other column cuts its page with
//! `Vector::slice`.
//!
//! The cut has to be `Vector::slice` and not `Vector::gather`, and the difference is the reason
//! `slice` exists. A gather walks a dictionary to its leaf and copies, so a gathered piece of a
//! dictionary encoded column arrives flat, and since a writer puts two thousand rows in a page
//! while a chunk holds a thousand and twenty four, almost every page of every dictionary column
//! gets cut. Gathering would have meant no dictionary column ever reaching an operator as a
//! dictionary, which is the form a group by over one is fast because of.
//!
//! # What this does not do yet
//!
//! One read per column chunk, synchronous, through `File::read_exact_at`. A row group's worth of
//! reads is meant to go through the submission interface in `rudb-io` as one batch, which is
//! `spec/engine/05-scan.md` section 5.3, and [`Pages`] was already written to take a chunk as one
//! slice so that it fits that shape without changing. Wiring it is the next change rather than this
//! one.
//!
//! Row group statistics are in the footer and are not consulted. Pruning needs a predicate and
//! there is nothing to push down until the table function exists, so the counter that would prove
//! pruning works is here and the pruning is E2.

use std::collections::VecDeque;

use rudb_common::{Error, Field, Result};
use rudb_compress::Codec;
use rudb_io::File;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::chunk::Pages;
use crate::metadata::Metadata;
use crate::page::Body;

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
    /// If the file is not Parquet, or its footer does not parse, or its schema is one this crate
    /// refuses, which today means a nested one.
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
    /// The order is the caller's and not the file's, so a query that selects the last column and
    /// then the first gets them that way round without a projection above the scan. A column named
    /// twice is read twice, because that is what a query asking for it twice means.
    ///
    /// # Errors
    ///
    /// If a column index is not one the file has. Taking indices rather than names is deliberate:
    /// resolving a name is the binder's job and it has rules about case and quoting that a file
    /// reader should not be having its own opinions about.
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
    /// A column the file marked `REQUIRED` comes back as `NOT NULL`, because the writer promised it
    /// and a reader has no reason to be vaguer about the data than the file was.
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

    /// How many bytes of column data have been read.
    ///
    /// The footer is not counted. It is read once whatever the query is, and what a projection test
    /// wants is the number that moves when the projection changes. `spec/engine/05-scan.md` section
    /// 5.6 asks for this next to every query for a reason: a scan that reads a column nobody asked
    /// for still returns the right answer, so the bug is invisible in the result and obvious here.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.bytes
    }

    /// The next chunk, or nothing when the file is done.
    ///
    /// # Errors
    ///
    /// If a read fails, a page does not decode, or the file uses a codec, an encoding or a type
    /// this build does not read yet. Every one of those is named in the error.
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

    /// Reads one row group and queues the chunks it holds.
    fn read_group(&mut self, at: usize) -> Result<()> {
        let group = &self.metadata.row_groups[at];
        let rows = usize::try_from(group.rows)
            .map_err(|_| Error::io(format!("a row group of {} rows", group.rows)))?;
        if rows == 0 {
            return Ok(());
        }
        // Where every projected chunk is, worked out before any of them is read, so that the walk
        // over the footer is done with by the time the reads start borrowing the reader mutably.
        let mut plan = Vec::with_capacity(self.projection.len());
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
            plan.push(Where {
                column: wanted,
                start: chunk.start(),
                len,
                codec: chunk.compression,
                values: chunk.values,
            });
        }
        let mut columns = Vec::with_capacity(plan.len());
        for chunk in plan {
            columns.push(Cursor::new(self.read_column(&chunk)?));
        }
        self.queue(columns, rows)
    }

    /// Reads one column chunk of one row group and decodes every page of it.
    ///
    /// The dictionary page is read first and kept for the whole walk, because one dictionary serves
    /// every data page of the chunk and the format puts it first for exactly that reason. It is not
    /// a row of the column and does not become a vector of its own.
    fn read_column(&mut self, chunk: &Where) -> Result<Vec<Vector>> {
        let mut bytes = vec![0_u8; chunk.len];
        self.file.read_exact_at(chunk.start, &mut bytes)?;
        self.bytes += chunk.len as u64;

        let column = &self.metadata.schema[chunk.column];
        let mut dictionary = None;
        let mut pages = Vec::new();
        for page in Pages::new(&bytes, chunk.codec, chunk.values) {
            let page = page?;
            if matches!(page.header.body, Body::Index) {
                continue;
            }
            if matches!(page.header.body, Body::Dictionary(_)) {
                if dictionary.is_some() {
                    return Err(Error::io(format!(
                        "a second dictionary page in the chunk for column {}",
                        column.name
                    )));
                }
                dictionary = Some(page.into_dictionary(column)?);
                continue;
            }
            pages.push(page.into_vector(column, dictionary.as_ref())?);
        }
        Ok(pages)
    }

    /// Zips the decoded columns of one row group into chunks.
    ///
    /// A projection of no columns has no vectors to take a length from, so the lengths come from
    /// the row count instead. That is the `SELECT count(*)` path and it is the only one where a
    /// chunk's length is not a property of anything inside it.
    fn queue(&mut self, mut columns: Vec<Cursor>, rows: usize) -> Result<()> {
        if columns.is_empty() {
            let mut left = rows;
            while left > 0 {
                let len = left.min(VECTOR_SIZE);
                self.ready.push_back(Chunk::with_rows(Vec::new(), len)?);
                left -= len;
            }
            return Ok(());
        }
        let mut done = 0;
        while done < rows {
            let mut len = (rows - done).min(VECTOR_SIZE);
            for column in &mut columns {
                len = len.min(column.left());
            }
            if len == 0 {
                return Err(Error::io(format!(
                    "a row group of {rows} rows whose columns ran out after {done} of them"
                )));
            }
            let mut vectors = Vec::with_capacity(columns.len());
            for column in &mut columns {
                vectors.push(column.take(len)?);
            }
            self.ready.push_back(Chunk::with_rows(vectors, len)?);
            done += len;
        }
        Ok(())
    }
}

/// Where one projected column chunk is and what it takes to read it.
///
/// Copied out of the footer before any reading starts. The footer is borrowed from the reader and
/// the read needs the reader mutably, so the two cannot overlap, and five fields are cheaper to
/// copy than the alternatives are to explain.
#[derive(Debug)]
struct Where {
    column: usize,
    start: u64,
    len: usize,
    codec: Codec,
    values: i64,
}

/// One column's decoded pages, and how far into them the reader has got.
///
/// The pages are held in reverse so that taking the next one is a pop rather than a shift, which
/// matters only because it lets the vector be moved out instead of cloned.
#[derive(Debug)]
struct Cursor {
    pages: Vec<Vector>,
    at: usize,
}

impl Cursor {
    /// A cursor over a column's pages, in order.
    fn new(mut pages: Vec<Vector>) -> Self {
        pages.reverse();
        Self { pages, at: 0 }
    }

    /// How many rows are left in the page the cursor is in, stepping over any that are empty.
    ///
    /// Zero when the column has no pages left, which the caller turns into an error, because a
    /// column that runs out before the row group does is a column shorter than the one beside it.
    fn left(&mut self) -> usize {
        while let Some(page) = self.pages.last() {
            if self.at < page.len() {
                return page.len() - self.at;
            }
            self.pages.pop();
            self.at = 0;
        }
        0
    }

    /// The next `rows` rows, which is the whole page when that is exactly what is left.
    ///
    /// Handing the page over whole moves the vector rather than copying it, and that is the case
    /// worth having, but it is not the common one. A writer puts two thousand rows in a page and a
    /// chunk holds a thousand and twenty four, so most pages get cut. `Vector::slice` is what makes
    /// that cut affordable: it keeps a dictionary encoded page a dictionary vector, where a gather
    /// would have flattened it and thrown away the form the group by wants.
    fn take(&mut self, rows: usize) -> Result<Vector> {
        let page = self
            .pages
            .last()
            .ok_or_else(|| Error::internal("a parquet column asked for rows it has not got"))?;
        if self.at == 0 && rows == page.len() {
            return self.pages.pop().ok_or_else(|| Error::internal("a page that vanished"));
        }
        let piece = page.slice(self.at, rows)?;
        self.at += rows;
        Ok(piece)
    }
}

/// Reads a whole file into chunks, projecting every column.
///
/// The convenience a test wants and the thing a scan must not do, because it holds the whole file
/// in memory at once. It is here rather than written out in each test so that the loop over
/// [`Reader::next_chunk`] exists in one place.
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

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::Vector;

    use super::Cursor;

    fn ints(values: &[i32]) -> Vector {
        let values: Vec<Value> = values.iter().map(|&v| Value::Integer(v)).collect();
        Vector::from_values(LogicalType::Integer, &values).expect("builds")
    }

    #[test]
    fn a_page_taken_whole_is_the_same_vector_and_not_a_copy_of_it() {
        let mut cursor = Cursor::new(vec![ints(&[1, 2, 3])]);
        assert_eq!(cursor.left(), 3);
        let page = cursor.take(3).expect("takes the page");
        assert_eq!(page.len(), 3);
        assert_eq!(cursor.left(), 0, "the column has no pages left");
    }

    #[test]
    fn a_page_taken_in_pieces_comes_back_in_order() {
        let mut cursor = Cursor::new(vec![ints(&[1, 2, 3, 4])]);
        let first = cursor.take(3).expect("takes three");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            [Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
        assert_eq!(cursor.left(), 1);
        let second = cursor.take(1).expect("takes the rest");
        assert_eq!(second.value_at(0), Value::Integer(4));
    }

    #[test]
    fn the_cursor_walks_from_one_page_to_the_next() {
        let mut cursor = Cursor::new(vec![ints(&[1, 2]), ints(&[3])]);
        assert_eq!(cursor.left(), 2, "the first page is the one it is in");
        let _ = cursor.take(2).expect("takes the first page");
        assert_eq!(cursor.left(), 1, "and then the second");
        assert_eq!(cursor.take(1).expect("takes it").value_at(0), Value::Integer(3));
        assert_eq!(cursor.left(), 0);
    }

    #[test]
    fn an_empty_page_is_stepped_over_rather_than_returned_as_a_chunk_of_no_rows() {
        let mut cursor = Cursor::new(vec![ints(&[]), ints(&[7])]);
        assert_eq!(cursor.left(), 1);
        assert_eq!(cursor.take(1).expect("takes it").value_at(0), Value::Integer(7));
    }

    #[test]
    fn a_column_asked_for_rows_it_does_not_have_is_an_error_and_not_a_panic() {
        let mut cursor = Cursor::new(Vec::new());
        assert_eq!(cursor.left(), 0);
        assert!(cursor.take(1).is_err());
    }
}
