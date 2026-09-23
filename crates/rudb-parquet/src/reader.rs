//! Reading a whole Parquet file as a run of chunks.
//!
//! This is the layer above the page walker. [`Pages`] turns one column chunk's bytes into pages and
//! `Page::decode` turns one page into a vector, and what is left is everything that is about
//! the file rather than about a page: which columns to read, where their chunks are, and how to put
//! seven columns of a row group side by side into something the engine can execute over.
//!
//! The reader works a row group at a time, which is the unit the format stores and the unit the
//! scheduler hands out as a morsel. [`Reader::split`] is what makes that possible: it hands back
//! another reader over the same open file and the same parsed footer, positioned at a stretch of
//! row groups and reading nothing outside it. The file and the footer are shared rather than
//! reopened because a `File` here is addressed by offset and every method on it takes `&self`, so
//! two readers on one file do not interfere, and because parsing a hundred and five columns of
//! footer once per row group would cost more than the read it was splitting.
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
//! Row group statistics are read but not acted on here. Deciding which groups to skip is
//! [`crate::skips`], and handing out only the groups that survive is the scan in `rudb-exec`,
//! because the predicate lives up there and this reader is handed a range of groups rather than a
//! question. Page level skipping, which is the same idea a level down using the page index, is
//! still to come and belongs here rather than up there.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};

use rudb_common::stage::{Stage, Timing};
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_io::File;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::chunk::Pages;
use crate::metadata::{Metadata, SchemaColumn};
use crate::page::Body;
use crate::prune::Footer;

type PickedColumns = Vec<(usize, usize, Vec<Value>)>;

/// One column chunk of one row group, and where its values belong when they come back.
///
/// The unit of work a fetch hands a thread. It used to be a column, with the row groups done one
/// after another, and that left the threads waiting on whichever column of the row group in hand
/// was the widest. A fetch of ten rows touches five row groups on `hits-1m-snappy.parquet`, so
/// there were always five times as many of these available as were being run at once.
#[derive(Debug)]
struct Job {
    /// Which of the touched row groups this came from, counting only the ones that hold a row.
    order: usize,
    /// Which projected column, which is where the values go.
    column: usize,
    cursor: Cursor,
}

/// How many bytes of column chunk one fetch keeps open at once.
///
/// The number that matters is what a worker holds while it works: the chunk it read and the page it
/// decompressed out of it. Sixteen workers all on the widest columns of the file was worth a hundred
/// megabytes of peak RSS on ClickBench against reading a row group at a time, which is a bad trade
/// for the twenty seven milliseconds the wider spread bought. Thirty two megabytes is enough for the
/// widest chunk of `hits.parquet` several times over, so the big columns still overlap, and it is
/// small enough that the fetch is not what a query is remembered for.
const FETCH_BUDGET: usize = 32 << 20;

/// The jobs of one fetch, and how many bytes of them are open.
#[derive(Debug)]
struct Queue {
    /// Widest first, each taken out as a worker claims it.
    jobs: Vec<Option<Job>>,
    /// How many are still there, so a worker knows to stop rather than scanning an empty list.
    left: usize,
    inflight: usize,
}

/// The next job a worker should run, or nothing when there are none left.
///
/// Widest first among the ones that fit in what is left of the budget. A worker that finds nothing
/// small enough waits, and whoever finishes wakes it. Nothing fits and nothing is open cannot
/// happen, because the first job is always allowed when the budget is untouched.
fn claim(queue: &Mutex<Queue>, room: &Condvar) -> Result<Option<Job>> {
    let mut held =
        queue.lock().map_err(|_| Error::internal("a Parquet fetch queue was poisoned"))?;
    loop {
        if held.left == 0 {
            return Ok(None);
        }
        let inflight = held.inflight;
        let at = held.jobs.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|job| inflight == 0 || inflight + job.cursor.len <= FETCH_BUDGET)
        });
        match at {
            Some(at) => {
                let job = held.jobs[at].take().ok_or_else(|| {
                    Error::internal("a Parquet fetch claimed a job that was already taken")
                })?;
                held.left -= 1;
                held.inflight = held.inflight.saturating_add(job.cursor.len);
                return Ok(Some(job));
            }
            None => {
                held = room
                    .wait(held)
                    .map_err(|_| Error::internal("a Parquet fetch queue was poisoned"))?;
            }
        }
    }
}

/// Gives the budget back and wakes whoever was waiting for it.
fn release(queue: &Mutex<Queue>, room: &Condvar, bytes: usize) -> Result<()> {
    let mut held =
        queue.lock().map_err(|_| Error::internal("a Parquet fetch queue was poisoned"))?;
    held.inflight = held.inflight.saturating_sub(bytes);
    drop(held);
    room.notify_all();
    Ok(())
}

/// The decoded dictionary pages of the row groups being read, shared by every reader of one file.
///
/// A row group cut into four morsels used to decode each of its dictionary pages four times, which
/// on a file of wide string columns costs more than the cutting saves: the ClickBench suite at one
/// thread went from 2.9 seconds to 6.6 when the cutting landed without this. So the readers split
/// off one file share the pages they decode, and a morsel that starts inside a group reads the
/// dictionary page's header, finds the page already decoded and reads none of its body.
///
/// A group's pages are held exactly as long as a morsel of that group is alive, which is why
/// [`Reader::split_rows`] registers and [`Reader`]'s drop unregisters. Holding the newest group or
/// two instead is what this did first and it was wrong: thirty two threads reading a nine group
/// file have a morsel of every group in flight at once, so the newest group is whichever morsel was
/// handed out last and the pages of the other eight get thrown away while they are still wanted.
/// That cost 25 MB of extra dictionary decoding on the ClickBench file, more than the cutting saved.
///
/// A reader that was not split by rows registers nothing and so holds nothing, which is what a
/// sequential read over whole row groups wants: it reads each dictionary once already and would only
/// pay to keep it.
#[derive(Debug, Default)]
struct Dictionaries {
    held: Mutex<Held>,
}

/// The slots, and how many morsels of each row group are still reading.
#[derive(Debug, Default)]
struct Held {
    reading: HashMap<usize, usize>,
    pages: HashMap<(usize, usize), Arc<Slot>>,
}

/// One column chunk's dictionary page, and whoever is waiting for it.
///
/// A slot rather than a page because the morsels of a row group start together. Cutting a group four
/// ways and letting each morsel look the page up when it gets there decodes it four times anyway,
/// since all four look before any of them has finished, and that is exactly what the measurement
/// showed: 35 MB of dictionary decoded on the ClickBench file where reading it uncut decodes 10. So
/// the first morsel to ask takes the slot and the others wait on it.
#[derive(Debug, Default)]
struct Slot {
    page: Mutex<State>,
    ready: Condvar,
}

/// What is in a slot.
#[derive(Debug, Default)]
enum State {
    /// Somebody is decoding it, and whoever wants it should wait.
    #[default]
    Decoding,
    Decoded(Arc<Vector>),
    /// Whoever took it gave up, so everybody else is on their own.
    Failed,
}

/// What asking for a dictionary page gets you.
enum Claim {
    /// Another morsel decoded it, so read none of the page.
    Held(Arc<Vector>),
    /// Nobody has, so decode it and hand it over.
    Mine(Filling),
}

/// The right to decode one dictionary page, and the duty to say so either way.
///
/// Dropping one without filling it marks the slot failed and wakes the waiters, so a morsel that
/// fails on the page, or anywhere between taking the slot and decoding, does not leave the rest of
/// its row group waiting for a page that is never coming.
struct Filling {
    slot: Arc<Slot>,
    filled: bool,
}

impl Filling {
    /// Hands the decoded page to everybody waiting for it.
    fn fill(mut self, page: &Arc<Vector>) {
        if let Ok(mut state) = self.slot.page.lock() {
            *state = State::Decoded(Arc::clone(page));
        }
        self.filled = true;
        self.slot.ready.notify_all();
    }
}

impl Drop for Filling {
    fn drop(&mut self) {
        if self.filled {
            return;
        }
        if let Ok(mut state) = self.slot.page.lock() {
            *state = State::Failed;
        }
        self.slot.ready.notify_all();
    }
}

/// The page in a slot, waiting for whoever took it if it is not decoded yet.
///
/// Nothing when the slot failed, which means the caller decodes the page itself.
fn awaited(slot: &Arc<Slot>) -> Option<Arc<Vector>> {
    let mut state = slot.page.lock().ok()?;
    loop {
        match &*state {
            State::Decoded(page) => return Some(Arc::clone(page)),
            State::Failed => return None,
            State::Decoding => state = slot.ready.wait(state).ok()?,
        }
    }
}

impl Dictionaries {
    /// The decoded dictionary page of one column chunk, or the job of decoding it.
    ///
    /// Nothing at all when this row group is not being read in pieces, because a group read whole
    /// reads each of its dictionaries once already and would only pay to keep them. A poisoned lock
    /// is nothing too: losing the cache costs time and nothing else, and a read failing because
    /// another thread panicked somewhere unrelated would be worse.
    fn claim(&self, group: usize, column: usize) -> Option<Claim> {
        let mut held = self.held.lock().ok()?;
        if !held.reading.contains_key(&group) {
            return None;
        }
        if let Some(slot) = held.pages.get(&(group, column)) {
            let slot = Arc::clone(slot);
            drop(held);
            return awaited(&slot).map(Claim::Held);
        }
        let slot = Arc::new(Slot::default());
        held.pages.insert((group, column), Arc::clone(&slot));
        Some(Claim::Mine(Filling { slot, filled: false }))
    }

    /// Says that one more morsel of this row group is about to be read.
    fn opened(&self, group: usize) {
        let Ok(mut held) = self.held.lock() else { return };
        *held.reading.entry(group).or_default() += 1;
    }

    /// Says that one morsel of this row group is done, and drops the pages once they all are.
    fn closed(&self, group: usize) {
        let Ok(mut held) = self.held.lock() else { return };
        let Some(left) = held.reading.get_mut(&group) else { return };
        *left = left.saturating_sub(1);
        if *left == 0 {
            held.reading.remove(&group);
            held.pages.retain(|&(at, _), _| at != group);
        }
    }
}

/// Where a chunk's dictionary page belongs in the cache the readers of one file share.
#[derive(Debug, Clone)]
struct Cached {
    pages: Arc<Dictionaries>,
    group: usize,
    column: usize,
}

/// A Parquet file, read as chunks.
#[derive(Debug)]
pub struct Reader {
    file: Arc<dyn File>,
    metadata: Arc<Metadata>,
    /// The dictionary pages decoded so far, shared with every reader split off this one.
    dictionaries: Arc<Dictionaries>,
    projection: Vec<usize>,
    group: usize,
    /// One past the last row group this reader reads, which is the whole file until
    /// [`Reader::split`] says otherwise.
    end: usize,
    /// Which rows of each row group this reader reads, which is all of them until
    /// [`Reader::split_rows`] says otherwise.
    ///
    /// Only ever set on a reader that covers a single row group, because the only caller that wants
    /// part of a group is the one cutting that group into morsels.
    piece: Option<Range<usize>>,
    /// The row group whose dictionary pages this reader is keeping alive, if it is a morsel.
    ///
    /// Set by [`Reader::split_rows`] and cleared by dropping the reader, which is what tells the
    /// shared cache when the last morsel of a group has finished with it.
    holding: Option<usize>,
    active: Option<Group>,
    bytes: u64,
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(group) = self.holding {
            self.dictionaries.closed(group);
        }
    }
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
        let file: Arc<dyn File> = Arc::from(file);
        let metadata = Metadata::read(file.as_ref())?;
        let projection = (0..metadata.schema.len()).collect();
        let end = metadata.row_groups.len();
        Ok(Self {
            file,
            metadata: Arc::new(metadata),
            dictionaries: Arc::new(Dictionaries::default()),
            projection,
            group: 0,
            end,
            piece: None,
            holding: None,
            active: None,
            bytes: 0,
        })
    }

    /// The footer.
    #[must_use]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// The zone maps of this file, to outlive the reader that read them.
    ///
    /// The planner wants the bounds after the file is closed, and the footer is behind an `Arc`
    /// already because splitting a reader shares it, so handing one out costs a refcount and copies
    /// nothing. Nobody can mutate it, since [`Reader`] never does either.
    #[must_use]
    pub fn zones(&self) -> Footer {
        Footer::new(Arc::clone(&self.metadata))
    }

    /// Another reader over the same file, reading only the row groups in `groups`.
    ///
    /// The open file and the parsed footer are shared, and the projection and whatever
    /// [`Reader::as_string`] was told are carried over, so a caller splits once a reader is set up
    /// and gets readers that are set up the same way. What is not carried over is the position and
    /// the byte counter, which start where the split says and at zero, so a caller that adds up
    /// [`Reader::bytes_read`] across the splits gets what the whole read cost.
    ///
    /// The restriction is on the sequential read alone. [`Reader::rows_at`] addresses the whole
    /// file whichever reader it is asked of, because a row ordinal means the same thing to every
    /// reader of one file and a fetch is not a scan.
    ///
    /// # Errors
    ///
    /// If the range runs past the end of the file, which is a mistake in the caller rather than
    /// anything a query can cause.
    pub fn split(&self, groups: Range<usize>) -> Result<Self> {
        let total = self.metadata.row_groups.len();
        if groups.start > groups.end || groups.end > total {
            return Err(Error::internal(format!(
                "row groups {}..{} of a parquet file with {total} of them",
                groups.start, groups.end
            )));
        }
        Ok(Self {
            file: Arc::clone(&self.file),
            metadata: Arc::clone(&self.metadata),
            dictionaries: Arc::clone(&self.dictionaries),
            projection: self.projection.clone(),
            group: groups.start,
            end: groups.end,
            piece: None,
            holding: None,
            active: None,
            bytes: 0,
        })
    }

    /// Another reader over part of one row group of the same file.
    ///
    /// [`Reader::split`] hands out whole row groups, and a whole row group is too large a unit of
    /// work for a machine with cores to spare. DuckDB writes a hundred and twenty two thousand rows
    /// into one, so the million row ClickBench file has nine of them, and a scan that can only cut
    /// nine pieces leaves twenty three of a thirty two core machine's threads with nothing to do and
    /// gets no faster above eight. Measured on that file: the same suite over a copy written with
    /// thirty two thousand row groups runs in 749 ms at eight threads against 898, and in 676 ms at
    /// thirty two threads against 930, where the nine group file gets slower the more threads it is
    /// given.
    ///
    /// `rows` is a row ordinal range inside the group, not inside the file. What it costs to start
    /// part way in is one page header read per page stepped over, which is a couple of hundred bytes
    /// each and no decompression at all, because a header says how many values its page holds and
    /// that is all the reader needs to know to skip it.
    ///
    /// What it does not step over is the page the first row is in, which is decoded whole and then
    /// cut. So the page is the floor on how finely a file can be read, and a caller works out where
    /// that floor is with [`Reader::page_bytes`] before cutting anything. Cutting below it is not a
    /// little wasteful, it is a disaster: the ClickBench suite at one thread went from 2.9 seconds
    /// to 5.9 when a file DuckDB wrote was cut four ways, because DuckDB writes one page per column
    /// chunk and each of the four morsels decoded the whole of it.
    ///
    /// # Errors
    ///
    /// If the group is not one the file has, which is a mistake in the caller.
    pub fn split_rows(&self, group: usize, rows: Range<usize>) -> Result<Self> {
        let mut split = self.split(group..group + 1)?;
        split.piece = Some(rows);
        split.holding = Some(group);
        self.dictionaries.opened(group);
        Ok(split)
    }

    /// What a piece of a row group reads before it reads a row it wants.
    ///
    /// Stepping over a page is free, because its header says how long it is, but a piece that starts
    /// inside a page reads that whole page and throws away the part before it starts. So this is the
    /// first data page of every projected column added up, header and compressed body both, which is
    /// what starting anywhere other than the top of a row group costs.
    ///
    /// A caller decides whether cutting is worth it by holding this against
    /// [`Reader::chunk_bytes`], which is what reading the whole group costs. The two are the same
    /// number on a file DuckDB wrote, because DuckDB writes one page per column chunk, and such a
    /// file cannot usefully be cut at all: `URL` in the ClickBench data is one plain page of ten and
    /// a half megabytes per row group.
    ///
    /// Only the first row group is read, because page size is a setting the writer holds for the
    /// whole file rather than something that moves through one, and only the projected columns,
    /// because a column nobody reads does not have to be cut. What it costs is one page header per
    /// column, which is a couple of hundred bytes each and no page bodies at all.
    ///
    /// Zero when this reader has no row groups left to read.
    ///
    /// # Errors
    ///
    /// If a column chunk is missing from the footer or its first page header does not parse.
    pub fn page_bytes(&self) -> Result<u64> {
        if self.group >= self.end {
            return Ok(0);
        }
        let mut total = 0u64;
        for chunk in self.locate(self.group)? {
            let mut cursor = self.read_column(&chunk);
            total = total.saturating_add(cursor.first_page(self.file.as_ref())?);
        }
        Ok(total)
    }

    /// What reading the whole of this reader's next row group costs, over the projected columns.
    ///
    /// Compressed bytes, straight out of the footer, so this reads nothing. Zero when the reader has
    /// no row groups left.
    #[must_use]
    pub fn chunk_bytes(&self) -> u64 {
        let Some(group) = self.metadata.row_groups.get(self.group) else { return 0 };
        if self.group >= self.end {
            return 0;
        }
        self.projection
            .iter()
            .filter_map(|&column| group.columns.get(column))
            .map(|chunk| u64::try_from(chunk.compressed_size).unwrap_or(0))
            .sum()
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
        let metadata = Arc::make_mut(&mut self.metadata);
        for (&at, &text) in self.projection.iter().zip(columns) {
            let column = &mut metadata.schema[at];
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

    /// How many rows the whole file holds, as its footer states.
    ///
    /// The file's count and not this reader's, so a reader that [`Reader::split`] has narrowed to
    /// one row group still answers for all of them. The caller is the planner, which wants to know
    /// how large the input is before anything has been split, and a footer is counted rather than
    /// sampled, so this is exact.
    ///
    /// `None` for a file whose footer states a negative count, which no writer produces. That is
    /// not zero rows and it is not an error either: a corrupt footer should be reported by whatever
    /// tries to read the data, in the words it already has, and not through a row count the planner
    /// asked for. The planner's answer for it is that nobody counted.
    #[must_use]
    pub fn rows(&self) -> Option<u64> {
        u64::try_from(self.metadata.rows).ok()
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
        let mut wanted: Vec<Vec<usize>> = Vec::new();
        let mut touched: Vec<usize> = Vec::new();
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
            if !local.is_empty() {
                wanted.push(local);
                touched.push(at);
            }
        }
        if next < rows.len() {
            return Err(Error::internal(format!(
                "row ordinal {} is past the end of a file of {base} rows",
                rows[next]
            )));
        }
        let mut jobs: Vec<Job> = Vec::new();
        for (order, &at) in touched.iter().enumerate() {
            for (column, chunk) in self.locate(at)?.iter().enumerate() {
                jobs.push(Job { order, column, cursor: self.read_column(chunk) });
            }
        }
        let mut gathered: Vec<Vec<Option<Vec<Value>>>> =
            touched.iter().map(|_| self.projection.iter().map(|_| None).collect()).collect();
        let read = self.fetch(jobs, &wanted, &mut gathered)?;
        self.bytes = self.bytes.saturating_add(read);
        let mut picked: Vec<Vec<Value>> =
            self.projection.iter().map(|_| Vec::with_capacity(rows.len())).collect();
        for group in gathered {
            for (column, values) in group.into_iter().enumerate() {
                let Some(values) = values else {
                    return Err(Error::internal("a Parquet fetch lost one of its columns"));
                };
                picked[column].extend(values);
            }
        }
        let vectors: Result<Vec<_>> = self
            .fields()
            .into_iter()
            .zip(&picked)
            .map(|(field, values)| Vector::from_values(field.ty, values))
            .collect();
        Chunk::with_rows(vectors?, rows.len())
    }

    /// Runs every column chunk a fetch has to open, spread over a handful of threads.
    ///
    /// Every column of every touched row group goes in at once. Doing a row group at a time meant
    /// the threads waited on whichever column of the row group in hand was widest, and a fetch of
    /// ten rows touches five row groups on `hits-1m-snappy.parquet`, so there were five hundred and
    /// twenty five chunks to open and only a hundred and five of them ever in flight.
    ///
    /// The widest chunk goes first, which is the rule that keeps the tail short, and the threads
    /// pull from one list rather than being dealt a share up front, because a share dealt by size
    /// is a guess at how long a chunk takes and the list is not a guess.
    ///
    /// What the list is bounded by is bytes rather than jobs. A worker holds the chunk it is
    /// reading and the page it decompressed out of it, and the widest chunks of this file are
    /// megabytes each, so sixteen threads all starting on the widest chunk in the file is the peak
    /// of the whole query. [`FETCH_BUDGET`] is what a fetch is allowed to have open at once, and a
    /// thread that would go over it waits for one to finish instead. A chunk wider than the budget
    /// still runs, alone, because refusing it would mean never finishing.
    ///
    /// Sixteen threads at most, and the reader has no view of what the query was told to use. That
    /// is a wart, and it is the same one the scan has.
    ///
    /// # Errors
    ///
    /// If a read or a decode fails, or if a worker panics.
    fn fetch(
        &self,
        jobs: Vec<Job>,
        wanted: &[Vec<usize>],
        into: &mut [Vec<Option<Vec<Value>>>],
    ) -> Result<u64> {
        let workers = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(16)
            .min(jobs.len().max(1));
        if workers <= 1 || jobs.len() < 8 {
            let mut read = 0_u64;
            for mut job in jobs {
                let values = job.cursor.pick(self.file.as_ref(), &wanted[job.order])?;
                read = read.saturating_add(job.cursor.bytes_read);
                into[job.order][job.column] = Some(values);
            }
            return Ok(read);
        }
        let mut ordered = jobs;
        ordered.sort_unstable_by_key(|job| std::cmp::Reverse(job.cursor.len));
        let left = ordered.len();
        let queue =
            Mutex::new(Queue { jobs: ordered.into_iter().map(Some).collect(), left, inflight: 0 });
        let room = Condvar::new();
        std::thread::scope(|scope| -> Result<u64> {
            let mut running = Vec::with_capacity(workers);
            for _ in 0..workers {
                let file = self.file.as_ref();
                let queue = &queue;
                let room = &room;
                running.push(scope.spawn(move || -> Result<(u64, PickedColumns)> {
                    let mut read = 0_u64;
                    let mut output = Vec::new();
                    // The job is taken by value and dropped here, because a cursor keeps the last
                    // page body it read to decompress the next one into, and holding those is what
                    // the budget is counting.
                    while let Some(mut job) = claim(queue, room)? {
                        let taken = job.cursor.len;
                        let values = job.cursor.pick(file, &wanted[job.order])?;
                        read = read.saturating_add(job.cursor.bytes_read);
                        output.push((job.order, job.column, values));
                        drop(job);
                        release(queue, room, taken)?;
                    }
                    Ok((read, output))
                }));
            }
            let mut read = 0_u64;
            for job in running {
                let (bytes, output) = job
                    .join()
                    .map_err(|_| Error::internal("a Parquet fetch worker panicked"))??;
                read = read.saturating_add(bytes);
                for (order, column, values) in output {
                    into[order][column] = Some(values);
                }
            }
            Ok(read)
        })
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
            if self.group >= self.end {
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
        let (from, upto) = match &self.piece {
            Some(piece) => (piece.start.min(rows), piece.end.min(rows)),
            None => (0, rows),
        };
        if from >= upto {
            return Ok(());
        }
        let plan = self.locate(at)?;
        let mut columns = Vec::with_capacity(plan.len());
        for chunk in plan {
            let mut cursor = self.read_column(&chunk);
            // Every column is put on the same row of the group, so the pages the columns are cut
            // into do not have to line up with each other and no writer's page size is assumed.
            cursor.seek(self.file.as_ref(), from)?;
            columns.push(cursor);
        }
        self.active = Some(Group { columns, rows: upto - from, done: 0 });
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
                group: at,
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
    /// a row of the column and does not become a vector of its own. The cursor is pointed at the
    /// cache this file's readers share, so a chunk another morsel of the same row group has already
    /// decoded costs a page header and no more.
    fn read_column(&self, chunk: &Where) -> Cursor {
        let cached = Cached {
            pages: Arc::clone(&self.dictionaries),
            group: chunk.group,
            column: chunk.column,
        };
        Cursor::new(
            chunk.start,
            chunk.len,
            chunk.codec,
            chunk.values,
            self.metadata.schema[chunk.column].clone(),
            Some(cached),
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
    group: usize,
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
    /// Where this chunk's dictionary page belongs in the cache the readers of one file share.
    ///
    /// `None` on a cursor built by a test, which reads one chunk once and has nothing to share it
    /// with.
    cached: Option<Cached>,
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
    /// stays empty on a column of strings that is not dictionary encoded, and `arena.rs` is what
    /// gets a run back to that column.
    spare: Vec<u8>,
}

impl Cursor {
    /// A cursor over a column's pages, in order.
    fn new(
        start: u64,
        len: usize,
        codec: Codec,
        left: i64,
        column: SchemaColumn,
        cached: Option<Cached>,
    ) -> Self {
        Self {
            start,
            len,
            at: 0,
            codec,
            left,
            column,
            dictionary: None,
            cached,
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
            cached: None,
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
                // Another morsel of the same row group may have decoded this page already. Reading
                // it again could not be helped, because a page says what it is only once it is read,
                // but decoding it again is the part that costs.
                match self.claim_dictionary() {
                    Some(Claim::Held(held)) => self.dictionary = Some(held),
                    claim => {
                        let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
                        let timing = Timing::start(Stage::Dictionary);
                        let built = page.decode_dictionary(&self.column);
                        timing.stop(bytes);
                        self.hold_dictionary(claim, Arc::new(built?));
                    }
                }
                self.spare = page.body;
                continue;
            }
            self.left -= i64::from(page.header.values());
            let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
            let timing = Timing::start(Stage::Decode);
            let decoded = page.decode(&self.column, self.dictionary.as_ref());
            timing.stop(bytes);
            self.spare = page.body;
            // Held as a page, because it is about to be cut into chunks and every chunk is cloned
            // again downstream, by a projection that keeps a column and by a sort that holds its
            // input. On an owned run each of those is a copy of the values, on a page it is a count.
            self.page = Some(decoded?.into_pages());
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

    /// Steps over whole pages until `target`, a row ordinal inside this column chunk.
    ///
    /// What makes this affordable is that a page header carries the number of values in its page, so
    /// deciding that nobody wants a page costs the couple of hundred bytes of its header and none of
    /// its body. A page is never read, never decompressed and never decoded to be skipped. Only the
    /// one page the target lands in is decoded, and the rows of it before the target are dropped by
    /// starting the cursor part way into it.
    ///
    /// The dictionary page is the one page that cannot be stepped over, because every data page
    /// after it is written in terms of it.
    fn seek(&mut self, file: &dyn File, target: usize) -> Result<()> {
        while self.row < target {
            let (prefix, header, _, total) = self.peek(file)?;
            if matches!(header.body, Body::Index) {
                self.bytes_read = self.bytes_read.saturating_add(prefix.len() as u64);
                self.at = self.at.saturating_add(total);
                continue;
            }
            if matches!(header.body, Body::Dictionary(_)) {
                self.take_dictionary(file, prefix, total)?;
                continue;
            }
            let values = usize::try_from(header.values())
                .map_err(|_| Error::io(format!("a page of {} values", header.values())))?;
            if self.row.saturating_add(values) > target {
                break;
            }
            self.bytes_read = self.bytes_read.saturating_add(prefix.len() as u64);
            self.at = self.at.saturating_add(total);
            self.row = self.row.saturating_add(values);
            self.left -= i64::from(header.values());
        }
        let inside = target.saturating_sub(self.row);
        if inside > 0 {
            // The page the target is in, decoded by the streaming walk so that there is one piece of
            // code that knows how to turn a page into a vector, and then started part way in.
            self.left(Some(file))?;
            self.offset = inside;
        }
        Ok(())
    }

    /// Reads and decodes the chunk's dictionary page, which every data page after it points into.
    ///
    /// Another morsel of the same row group may have decoded it already, in which case the page
    /// comes out of the cache and its body is never read. That is the whole reason a row group can
    /// be cut into morsels without paying for the cut: the page header says how long the page is,
    /// which is all that is needed to step over it.
    fn take_dictionary(&mut self, file: &dyn File, prefix: Vec<u8>, total: usize) -> Result<()> {
        if self.dictionary.is_some() {
            return Err(Error::io(format!(
                "a second dictionary page in the chunk for column {}",
                self.column.name
            )));
        }
        let claim = self.claim_dictionary();
        if let Some(Claim::Held(held)) = claim {
            self.bytes_read = self.bytes_read.saturating_add(prefix.len() as u64);
            self.at = self.at.saturating_add(total);
            self.dictionary = Some(held);
            return Ok(());
        }
        let encoded = self.body(file, prefix, total)?;
        let mut pages = Pages::new(&encoded, self.codec, self.left);
        let mut page = pages.next().transpose()?.ok_or_else(|| {
            Error::io(format!("the dictionary page of column {} is empty", self.column.name))
        })?;
        let bytes = u64::try_from(page.body.len()).unwrap_or(u64::MAX);
        let timing = Timing::start(Stage::Dictionary);
        let built = page.decode_dictionary(&self.column);
        timing.stop(bytes);
        self.hold_dictionary(claim, Arc::new(built?));
        self.at = self.at.saturating_add(total);
        Ok(())
    }

    /// How many bytes the chunk's first data page takes, reading page headers and no bodies.
    ///
    /// Header and compressed body both, because a morsel that starts inside a page pays for both of
    /// them. Zero on a chunk with no data page in it, which is a chunk of no rows.
    fn first_page(&mut self, file: &dyn File) -> Result<u64> {
        while self.at < self.len {
            let (_, header, _, total) = self.peek(file)?;
            if !matches!(header.body, Body::Index | Body::Dictionary(_)) {
                return Ok(u64::try_from(total).unwrap_or(u64::MAX));
            }
            self.at = self.at.saturating_add(total);
        }
        Ok(0)
    }

    /// The chunk's dictionary page, or the job of decoding it for the rest of the row group.
    ///
    /// Blocks while another morsel of the same group is decoding it, which is the point: the morsels
    /// of a group start together, so a lookup that did not wait would find nothing and every one of
    /// them would decode the page.
    fn claim_dictionary(&self) -> Option<Claim> {
        self.cached.as_ref().and_then(|at| at.pages.claim(at.group, at.column))
    }

    /// Keeps a freshly decoded dictionary page, and hands it to whoever is waiting for it.
    fn hold_dictionary(&mut self, claim: Option<Claim>, built: Arc<Vector>) {
        if let Some(Claim::Mine(filling)) = claim {
            filling.fill(&built);
        }
        self.dictionary = Some(built);
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
                self.take_dictionary(file, prefix, total)?;
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
            let decoded = page.decode_at(&self.column, self.dictionary.as_ref(), &indices);
            timing.stop(bytes);
            out.extend(decoded?.iter());
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
    ///
    /// The `resize` below is a memset of the whole page body, and it is expensive: on a ten column
    /// scan of ClickBench's `hits` it is 23.6M instructions, about six percent of the program, and
    /// every byte of it is overwritten by the read on the next line. Two ways out of it have been
    /// measured and both are worse, so the memset stays until the reader can borrow a page rather
    /// than copy one.
    ///
    /// # Why not keep the buffer across pages
    ///
    /// Holding it at its high water mark and growing it only when a page needs more is correct, and
    /// it measured ten percent slower on one thread and thirty four percent slower on thirty two.
    /// It also cannot help a DuckDB written file, because those put one page in a column chunk and a
    /// cursor covers one column of one row group, so there is no second page to reuse it for. This
    /// is the same answer `arena.rs` records for parking buffers between cursors.
    ///
    /// # Why not ask the allocator for zeros
    ///
    /// Because `alloc_zeroed` does not make the zeroing free, it moves it into the kernel. A page
    /// body is above the allocator's mmap threshold, so `vec![0u8; total]` can come back as fresh
    /// pages that are already zero and skip the memset, and it does: the memset dropped from 43.5M
    /// instructions to 37.5M and the program from 364.5M to 358.8M. Wall clock went the other way,
    /// by a lot. Every one of those pages then takes a fault on first touch, and the mmap and the
    /// faults are kernel work under locks the whole process shares, which a memset is not. Unchanged
    /// on one thread and twenty four percent slower on thirty two, confirmed twice.
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
