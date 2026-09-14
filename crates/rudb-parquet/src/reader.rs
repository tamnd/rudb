//! Reading a whole Parquet file as a run of chunks.
//!
//! This is the layer above the page walker. [`Pages`] turns one column chunk's bytes into pages and
//! `Page::decode` turns one page into a vector, and what is left is everything that is about
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
//! # Where the time goes
//!
//! The reader charges itself to [`rudb_common::stage`], a clock per stage rather than one number
//! for the whole scan. Getting the bytes off the file is `read`, the codec is `decompress`, turning
//! a page into a vector is `decode`, building the dictionary the pages point into is `dictionary`,
//! and cutting pages to the chunk boundary is `assemble`. The shim above reads the difference
//! around each operator call, so the scan's row in the metrics document has a split under it and
//! the question of which quarter of a scan to work on has an answer rather than a guess.
//!
//! The clock is read once per page and once per chunk. What is not charged to any stage is the walk
//! itself, which is a page header decoded per page, and it shows up as the difference between the
//! operator's own time and the stages under it. That difference being large would itself be worth
//! knowing.
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

use std::sync::Arc;

use rudb_common::stage::{Stage, Timing};
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_io::File;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::chunk::Pages;
use crate::metadata::{Metadata, SchemaColumn};
use crate::page::Body;

/// A Parquet file, read as chunks.
#[derive(Debug)]
pub struct Reader {
    file: Box<dyn File>,
    metadata: Metadata,
    projection: Vec<usize>,
    group: usize,
    end_group: usize,
    active: Option<Group>,
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
        let end_group = metadata.row_groups.len();
        Ok(Self { file, metadata, projection, group: 0, end_group, active: None, bytes: 0 })
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
        self.active = None;
        Ok(())
    }

    /// Reads the projected columns marked here as text where the file left them as bytes.
    ///
    /// This is `binary_as_string`. A byte array column with no annotation on it is a `BLOB` here,
    /// because that is all the file said, and a caller who knows the writer meant text says so with
    /// this. Nothing about the read changes: the column already comes back in the one string column
    /// rudb has and its bytes are already validated on the way in, so the whole of the option is
    /// what the column is called.
    ///
    /// One flag per projected column rather than one for the file, so a call that asks for the same
    /// column twice and a call that asks for two of a hundred and five both line up with the answer
    /// the binder worked out. A flag on a column the file did not store as a byte array does
    /// nothing, since the caller is describing a file rather than asking for a conversion.
    pub fn as_string(&mut self, columns: &[bool]) {
        for (&at, &text) in self.projection.iter().zip(columns) {
            let column = &mut self.metadata.schema[at];
            if text && column.ty == LogicalType::Blob {
                column.ty = LogicalType::Varchar;
            }
        }
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

    /// Restricts subsequent streaming reads to one row group.
    ///
    /// This is the unit a parallel scan hands to one worker. The projection and text settings stay
    /// in place, so a worker opens and configures one reader and moves it between the row groups it
    /// is assigned rather than reading the footer again for every morsel.
    ///
    /// # Errors
    ///
    /// If `group` is outside the file.
    pub fn only_row_group(&mut self, group: usize) -> Result<()> {
        if group >= self.metadata.row_groups.len() {
            return Err(Error::io(format!(
                "row group {group} of a parquet file with {} row groups",
                self.metadata.row_groups.len()
            )));
        }
        self.group = group;
        self.end_group = group + 1;
        self.active = None;
        Ok(())
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

    /// The projected columns of the rows at `rows`, which are ordinals into the whole file.
    ///
    /// Sorted and strictly increasing, because the reader walks the file forwards once and a caller
    /// that wants them back in some other order can put them there.
    ///
    /// This is the fetch half of late materialisation, and what it is for is the query that reads
    /// one column of a hundred and five to decide which ten rows matter and then wants the other
    /// hundred and four columns of those ten rows. Scanning for them costs the whole file. Fetching
    /// them costs a row group's footer entry plus one page per column per row, because a row group
    /// that holds none of the wanted rows is never opened and a page that holds none of them has
    /// its header read and its body skipped.
    ///
    /// The answer is one chunk however many rows were asked for, so a caller with more rows than a
    /// chunk should hold is asking the wrong question. The rows that survive a `LIMIT` are the
    /// caller this exists for.
    ///
    /// # Errors
    ///
    /// If the ordinals are not sorted and strictly increasing, if one of them is past the end of
    /// the file, or if a read or a decode fails.
    pub fn rows_at(&mut self, rows: &[u64]) -> Result<Chunk> {
        for pair in rows.windows(2) {
            if pair[0] >= pair[1] {
                return Err(Error::internal(format!(
                    "row ordinals {} and {} are not sorted and strictly increasing",
                    pair[0], pair[1]
                )));
            }
        }
        let mut picked: Vec<Vec<Value>> =
            self.projection.iter().map(|_| Vec::with_capacity(rows.len())).collect();
        let mut base = 0_u64;
        let mut next = 0;
        for at in 0..self.metadata.row_groups.len() {
            if next >= rows.len() {
                break;
            }
            let count = u64::try_from(self.metadata.row_groups[at].rows).map_err(|_| {
                Error::io(format!("a row group of {} rows", self.metadata.row_groups[at].rows))
            })?;
            let end = base.saturating_add(count);
            let mut local = Vec::new();
            while next < rows.len() && rows[next] < end {
                local.push(usize::try_from(rows[next] - base).unwrap_or(usize::MAX));
                next += 1;
            }
            base = end;
            if local.is_empty() {
                continue;
            }
            let plan = self.locate(at)?;
            for (column, values) in plan.iter().zip(&mut picked) {
                let mut cursor = self.read_column(column);
                values.extend(cursor.pick(self.file.as_ref(), &local)?);
                self.bytes = self.bytes.saturating_add(cursor.bytes_read);
            }
        }
        if next < rows.len() {
            return Err(Error::internal(format!(
                "row ordinal {} is past the end of a file of {base} rows",
                rows[next]
            )));
        }
        let vectors: Result<Vec<_>> = self
            .fields()
            .into_iter()
            .zip(&picked)
            .map(|(field, values)| Vector::from_values(field.ty, values))
            .collect();
        Chunk::with_rows(vectors?, rows.len())
    }

    /// The next chunk, or nothing when the file is done.
    ///
    /// # Errors
    ///
    /// If a read fails, a page does not decode, or the file uses a codec, an encoding or a type
    /// this build does not read yet. Every one of those is named in the error.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        loop {
            if let Some(active) = &mut self.active {
                let (chunk, read) = active.next(self.file.as_ref())?;
                self.bytes = self.bytes.saturating_add(read);
                if let Some(chunk) = chunk {
                    return Ok(Some(chunk));
                }
                self.active = None;
            }
            if self.group >= self.end_group {
                return Ok(None);
            }
            let group = self.group;
            self.group += 1;
            self.read_group(group)?;
        }
    }

    /// Reads one row group and queues the chunks it holds.
    fn read_group(&mut self, at: usize) -> Result<()> {
        let group = &self.metadata.row_groups[at];
        let rows = usize::try_from(group.rows)
            .map_err(|_| Error::io(format!("a row group of {} rows", group.rows)))?;
        if rows == 0 {
            return Ok(());
        }
        let plan = self.locate(at)?;
        let mut columns = Vec::with_capacity(plan.len());
        for chunk in plan {
            columns.push(self.read_column(&chunk));
        }
        self.active = Some(Group { columns, rows, done: 0 });
        Ok(())
    }

    /// Where every projected column chunk of one row group is.
    ///
    /// Worked out before any of them is read, so that the walk over the footer is done with by the
    /// time the reads start borrowing the reader mutably.
    fn locate(&self, at: usize) -> Result<Vec<Where>> {
        let group = &self.metadata.row_groups[at];
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
        Ok(plan)
    }

    /// Reads one column chunk of one row group and decodes every page of it.
    ///
    /// The dictionary page is read first and kept for the whole walk, because one dictionary serves
    /// every data page of the chunk and the format puts it first for exactly that reason. It is not
    /// a row of the column and does not become a vector of its own.
    fn read_column(&self, chunk: &Where) -> Cursor {
        Cursor::new(
            chunk.start,
            chunk.len,
            chunk.codec,
            chunk.values,
            self.metadata.schema[chunk.column].clone(),
        )
    }
}

#[derive(Debug)]
struct Group {
    columns: Vec<Cursor>,
    rows: usize,
    done: usize,
}

impl Group {
    fn next(&mut self, file: &dyn File) -> Result<(Option<Chunk>, u64)> {
        let before: u64 = self.columns.iter().map(|column| column.bytes_read).sum();
        if self.done >= self.rows {
            return Ok((None, 0));
        }
        let mut len = (self.rows - self.done).min(VECTOR_SIZE);
        for column in &mut self.columns {
            len = len.min(column.left(Some(file))?);
        }
        if len == 0 {
            return Err(Error::io(format!(
                "a row group of {} rows whose columns ran out after {} of them",
                self.rows, self.done
            )));
        }
        let timing = Timing::start(Stage::Assemble);
        let vectors: Result<Vec<_>> =
            self.columns.iter_mut().map(|column| column.take(len)).collect();
        timing.stop(0);
        let vectors = vectors?;
        self.done += len;
        let after: u64 = self.columns.iter().map(|column| column.bytes_read).sum();
        Ok((Some(Chunk::with_rows(vectors, len)?), after.saturating_sub(before)))
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
    start: u64,
    len: usize,
    at: usize,
    codec: Codec,
    left: i64,
    column: SchemaColumn,
    /// The chunk's dictionary page, decoded once and shared with every data page in it.
    ///
    /// Behind a handle rather than held by value because every page of the chunk builds a vector
    /// pointing at it, and that vector holds its values in an `Arc` whatever the cursor does. So a
    /// cursor holding the dictionary by value meant copying the whole thing per page on the way
    /// into a handle nothing else was holding.
    dictionary: Option<Arc<Vector>>,
    page: Option<Vector>,
    queued: Vec<Vector>,
    offset: usize,
    /// The ordinal, inside this column chunk, of the first value of the page at [`Self::at`].
    ///
    /// Only the picking walk keeps this up to date, because only the picking walk needs to know
    /// where a page sits before deciding whether to read it. The streaming walk reads every page in
    /// order and never asks.
    row: usize,
    bytes_read: u64,
    /// The last page body this column decoded, to read the next page into.
    ///
    /// A column chunk is a few thousand pages of roughly one size, and a body on a real file is
    /// megabytes. Handing the same buffer back to the walker each time is worth more than anything
    /// inside the decompressor, because a body that size comes fresh from the kernel and is faulted
    /// in a four kilobyte page at a time before the codec has written a byte of it.
    ///
    /// Empty whenever the last decode kept the body, which is what a string column does: its values
    /// are the page, pointed at rather than copied. So this fills up on a column of integers and
    /// stays empty on a column of strings that is not dictionary encoded.
    spare: Vec<u8>,
}

impl Cursor {
    /// A cursor over a column's pages, in order.
    fn new(start: u64, len: usize, codec: Codec, left: i64, column: SchemaColumn) -> Self {
        Self {
            start,
            len,
            at: 0,
            codec,
            left,
            column,
            dictionary: None,
            page: None,
            queued: Vec::new(),
            offset: 0,
            row: 0,
            bytes_read: 0,
            spare: Vec::new(),
        }
    }

    #[cfg(test)]
    fn decoded(mut pages: Vec<Vector>) -> Self {
        pages.reverse();
        Self {
            start: 0,
            len: 0,
            at: 0,
            codec: Codec::Uncompressed,
            left: 0,
            column: SchemaColumn {
                name: "test".into(),
                physical: crate::metadata::Physical::Int32,
                ty: LogicalType::Integer,
                optional: false,
                width: 0,
            },
            dictionary: None,
            page: None,
            queued: pages,
            offset: 0,
            row: 0,
            bytes_read: 0,
            spare: Vec::new(),
        }
    }

    /// How many rows are left in the page the cursor is in, stepping over any that are empty.
    ///
    /// Zero when the column has no pages left, which the caller turns into an error, because a
    /// column that runs out before the row group does is a column shorter than the one beside it.
    fn left(&mut self, file: Option<&dyn File>) -> Result<usize> {
        while self.page.as_ref().is_none_or(|page| self.offset >= page.len()) {
            self.page = None;
            self.offset = 0;
            if let Some(page) = self.queued.pop() {
                self.page = Some(page);
                continue;
            }
            if self.left <= 0 {
                return Ok(0);
            }
            let file = file.ok_or_else(|| Error::internal("an encoded page with no file"))?;
            let read = Timing::start(Stage::Read);
            let encoded = self.read_page(file);
            read.stop(
                encoded.as_ref().map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
            );
            let encoded = encoded?;
            let mut pages = Pages::new(&encoded, self.codec, self.left);
            pages.recycle(std::mem::take(&mut self.spare));
            let mut page = pages.next().transpose()?.ok_or_else(|| {
                Error::io(format!(
                    "the column {} ran out with {} values left",
                    self.column.name, self.left
                ))
            })?;
            let consumed = pages.position();
            self.at = self.at.saturating_add(consumed);
            if matches!(page.header.body, Body::Index) {
                continue;
            }
            if matches!(page.header.body, Body::Dictionary(_)) {
                if self.dictionary.is_some() {
                    return Err(Error::io(format!(
                        "a second dictionary page in the chunk for column {}",
                        self.column.name
                    )));
                }
                let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
                let timing = Timing::start(Stage::Dictionary);
                let built = page.decode_dictionary(&self.column);
                timing.stop(bytes);
                self.spare = page.body;
                self.dictionary = Some(Arc::new(built?));
                continue;
            }
            self.left -= i64::from(page.header.values());
            let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
            let timing = Timing::start(Stage::Decode);
            let decoded = page.decode(&self.column, self.dictionary.as_ref());
            timing.stop(bytes);
            self.spare = page.body;
            self.page = Some(decoded?);
        }
        Ok(self.page.as_ref().map_or(0, |page| page.len() - self.offset))
    }

    /// The next `rows` rows, which is the whole page when that is exactly what is left.
    ///
    /// Handing the page over whole moves the vector rather than copying it, and that is the case
    /// worth having, but it is not the common one. A writer puts two thousand rows in a page and a
    /// chunk holds a thousand and twenty four, so most pages get cut. `Vector::slice` is what makes
    /// that cut affordable: it keeps a dictionary encoded page a dictionary vector, where a gather
    /// would have flattened it and thrown away the form the group by wants.
    fn take(&mut self, rows: usize) -> Result<Vector> {
        if self.page.is_none() {
            self.page = self.queued.pop();
        }
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| Error::internal("a parquet column asked for rows it has not got"))?;
        if self.offset == 0 && rows == page.len() {
            return self.page.take().ok_or_else(|| Error::internal("a page that vanished"));
        }
        let piece = page.slice(self.offset, rows)?;
        self.offset += rows;
        Ok(piece)
    }

    /// The values at `wanted`, which are row ordinals inside this column chunk, sorted.
    ///
    /// The point of the method is the pages it does not read. A page header says how many values
    /// the page holds, and a header is a couple of hundred bytes, so a cursor that has read one can
    /// decide that nobody wants any of those rows and add the page's size to its offset. Ten rows
    /// out of a million touch ten pages of each column and skip the rest, which is the difference
    /// between fetching a row and scanning for it.
    ///
    /// The values come back as [`Value`], one per wanted row, which is the shape a fetch wants and
    /// the wrong shape for anything large. That is deliberate. This is for the handful of rows a
    /// `LIMIT` left standing, and a caller with a lot of rows to pick should be scanning.
    fn pick(&mut self, file: &dyn File, wanted: &[usize]) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(wanted.len());
        let mut next = 0;
        while next < wanted.len() {
            let (prefix, header, _, total) = self.peek(file)?;
            if matches!(header.body, Body::Index) {
                self.bytes_read = self.bytes_read.saturating_add(prefix.len() as u64);
                self.at = self.at.saturating_add(total);
                continue;
            }
            if matches!(header.body, Body::Dictionary(_)) {
                if self.dictionary.is_some() {
                    return Err(Error::io(format!(
                        "a second dictionary page in the chunk for column {}",
                        self.column.name
                    )));
                }
                let encoded = self.body(file, prefix, total)?;
                let mut pages = Pages::new(&encoded, self.codec, self.left);
                let mut page = pages.next().transpose()?.ok_or_else(|| {
                    Error::io(format!(
                        "the dictionary page of column {} is empty",
                        self.column.name
                    ))
                })?;
                let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
                let timing = Timing::start(Stage::Dictionary);
                let built = page.decode_dictionary(&self.column);
                timing.stop(bytes);
                self.dictionary = Some(Arc::new(built?));
                self.at = self.at.saturating_add(total);
                continue;
            }
            let values = usize::try_from(header.values())
                .map_err(|_| Error::io(format!("a page of {} values", header.values())))?;
            let end = self.row.saturating_add(values);
            if wanted[next] >= end {
                self.bytes_read = self.bytes_read.saturating_add(prefix.len() as u64);
                self.at = self.at.saturating_add(total);
                self.row = end;
                self.left -= i64::from(header.values());
                continue;
            }
            let base = self.row;
            let mut indices = Vec::new();
            while next < wanted.len() && wanted[next] < end {
                indices.push(u32::try_from(wanted[next] - base).unwrap_or(u32::MAX));
                next += 1;
            }
            let encoded = self.body(file, prefix, total)?;
            let mut pages = Pages::new(&encoded, self.codec, self.left);
            let mut page = pages.next().transpose()?.ok_or_else(|| {
                Error::io(format!("a data page of column {} is empty", self.column.name))
            })?;
            let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
            let timing = Timing::start(Stage::Decode);
            let decoded = page.decode(&self.column, self.dictionary.as_ref());
            timing.stop(bytes);
            out.extend(decoded?.gather(&indices)?.iter());
            self.at = self.at.saturating_add(total);
            self.row = end;
            self.left -= i64::from(header.values());
        }
        Ok(out)
    }

    /// Reads exactly one encoded page, discovering its variable-width header with bounded probes.
    fn read_page(&mut self, file: &dyn File) -> Result<Vec<u8>> {
        let (prefix, _, _, total) = self.peek(file)?;
        self.body(file, prefix, total)
    }

    /// The header of the page the cursor is sitting on, without reading its body.
    ///
    /// Reading the header alone is what makes skipping a page cheap. A header is a couple of
    /// hundred bytes and says how many values the page holds, so a cursor that knows nobody wants
    /// any of those rows can add the page's size to its offset and never touch the body at all.
    ///
    /// The probe is bounded because a Thrift header has no length in front of it. Two hundred and
    /// fifty six bytes covers every header any writer emits, and the doubling is there for the one
    /// that does not.
    fn peek(&self, file: &dyn File) -> Result<(Vec<u8>, crate::page::Header, usize, usize)> {
        let remaining = self.len.saturating_sub(self.at);
        if remaining == 0 {
            return Err(Error::io(format!(
                "the column {} ran out of page bytes",
                self.column.name
            )));
        }
        let mut width = remaining.min(256);
        let (prefix, header, header_len) = loop {
            let mut prefix = vec![0_u8; width];
            file.read_exact_at(self.start + self.at as u64, &mut prefix)?;
            match crate::page::Header::read(&prefix) {
                Ok((header, header_len)) => break (prefix, header, header_len),
                Err(_) if width < remaining => width = remaining.min(width.saturating_mul(2)),
                Err(error) => return Err(error),
            }
        };
        let body = header.compressed_size as usize;
        let total = header_len.checked_add(body).ok_or_else(|| {
            Error::io(format!("a page in column {} has an impossible size", self.column.name))
        })?;
        if total > remaining {
            return Err(Error::io(format!(
                "a page of {total} bytes with only {remaining} bytes left in column {}",
                self.column.name
            )));
        }
        Ok((prefix, header, header_len, total))
    }

    /// The whole page, given the prefix a [`Self::peek`] already read.
    fn body(&mut self, file: &dyn File, prefix: Vec<u8>, total: usize) -> Result<Vec<u8>> {
        let mut encoded = Vec::with_capacity(total);
        encoded.extend_from_slice(&prefix[..prefix.len().min(total)]);
        if encoded.len() < total {
            let old = encoded.len();
            encoded.resize(total, 0);
            file.read_exact_at(self.start + self.at as u64 + old as u64, &mut encoded[old..])?;
        } else {
            encoded.truncate(total);
        }
        self.bytes_read = self.bytes_read.saturating_add(total as u64);
        Ok(encoded)
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
        let mut cursor = Cursor::decoded(vec![ints(&[1, 2, 3])]);
        assert_eq!(cursor.left(None).unwrap(), 3);
        let page = cursor.take(3).expect("takes the page");
        assert_eq!(page.len(), 3);
        assert_eq!(cursor.left(None).unwrap(), 0, "the column has no pages left");
    }

    #[test]
    fn a_page_taken_in_pieces_comes_back_in_order() {
        let mut cursor = Cursor::decoded(vec![ints(&[1, 2, 3, 4])]);
        let first = cursor.take(3).expect("takes three");
        assert_eq!(
            first.iter().collect::<Vec<_>>(),
            [Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
        assert_eq!(cursor.left(None).unwrap(), 1);
        let second = cursor.take(1).expect("takes the rest");
        assert_eq!(second.value_at(0), Value::Integer(4));
    }

    #[test]
    fn the_cursor_walks_from_one_page_to_the_next() {
        let mut cursor = Cursor::decoded(vec![ints(&[1, 2]), ints(&[3])]);
        assert_eq!(cursor.left(None).unwrap(), 2, "the first page is the one it is in");
        let _ = cursor.take(2).expect("takes the first page");
        assert_eq!(cursor.left(None).unwrap(), 1, "and then the second");
        assert_eq!(cursor.take(1).expect("takes it").value_at(0), Value::Integer(3));
        assert_eq!(cursor.left(None).unwrap(), 0);
    }

    #[test]
    fn an_empty_page_is_stepped_over_rather_than_returned_as_a_chunk_of_no_rows() {
        let mut cursor = Cursor::decoded(vec![ints(&[]), ints(&[7])]);
        assert_eq!(cursor.left(None).unwrap(), 1);
        assert_eq!(cursor.take(1).expect("takes it").value_at(0), Value::Integer(7));
    }

    #[test]
    fn a_column_asked_for_rows_it_does_not_have_is_an_error_and_not_a_panic() {
        let mut cursor = Cursor::decoded(Vec::new());
        assert_eq!(cursor.left(None).unwrap(), 0);
        assert!(cursor.take(1).is_err());
    }
}
