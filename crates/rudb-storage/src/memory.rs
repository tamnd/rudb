//! A table that lives in memory, which is what M0 stores rows in.
//!
//! This is not the storage format. There are no blocks, no compression and no buffer manager in
//! here, and every one of those is what the rest of this crate becomes at M2. What this is, is
//! somewhere for rows to be so that the binder and the executor can be written and tested against
//! something real, and a shape that the real thing can replace without the layers above it noticing:
//! a table is a sequence of chunks, a scan reads them in order, and a scan asks for the columns it
//! wants rather than all of them.
//!
//! The one thing it does get right on purpose is that a read is by chunk and by column, and not by
//! row. A row-at-a-time interface here would be an interface every operator above would grow
//! against, and unwinding that later is the rewrite this project exists to avoid.
//!
//! # Row groups
//!
//! What a scan reads and what the table stores are two different sizes, and they answer two
//! different questions. How many values an operator should work on at once is a question about L1
//! and L2, and the answer is [`VECTOR_SIZE`]. How many rows should sit together in one run of memory
//! is a question about how many places a scan has to jump to, and the answer is much larger.
//!
//! So the rows are held in groups of [`ROWS_PER_GROUP`], one page per column per group, and a chunk
//! is a window cut out of a page. A twenty million row table of two columns used to be 19,532 chunks
//! and about 39,000 buffers, and is now 163 groups and 326 pages. Nothing above the storage seam
//! sees it: the chunk numbering is what it always was, and [`MemoryTable::read`] still answers chunk
//! `n` with the same rows it used to. What changed is where those rows are, which is next to the
//! rows of the chunks either side of them.
//!
//! The directory that says where each chunk is, is kept rather than computed, because a chunk does
//! not have to arrive [`VECTOR_SIZE`] rows long and the arithmetic would be wrong the first time one
//! does not. It is three numbers a chunk and it is what lets a group that could not be laid end to
//! end sit next to one that could without anything else knowing the difference.
//!
//! # It does have statistics
//!
//! A zone map per chunk, and the chunk is the unit of pruning, because the whole value of a zone map
//! is how few rows it speaks for: the note at the top of `zone.rs` measures ClickBench query 37
//! skipping 75 percent of its chunks and running two and a half times faster, and a synopsis
//! covering a hundred and twenty times as many rows would skip almost nothing.
//!
//! There is a second level on top of them anyway, one per group, folded out of the chunk zones as
//! the group fills rather than walked for. What it buys is not selectivity, and measuring it as
//! though it were is how you talk yourself out of building it. What it buys is that a group it rules
//! out is a group whose hundred and twenty chunk zones are never asked at all, and asking one is not
//! free: before this, a `count(*)` with an equality that ruled out every chunk of a twenty million
//! row table still cost 16 milliseconds, which is 824 nanoseconds a chunk to decide to do nothing,
//! because the scan above was handed one chunk per morsel and had to return through the whole
//! pipeline between each of them. [`MemoryTable::group_parts`] is what lets it be handed a run
//! instead, and [`MemoryTable::group_skips`] is what lets it drop the run without walking it.
//!
//! What they are is built on the way in, which is `zone.rs`. They were put here to skip a chunk
//! a filter rules out, and they hold more than that: an exact null count for every column whatever
//! form it arrived in, the two ends, and the total of an integer column. So a `COUNT`, a `MIN`, a
//! `MAX`, a `SUM` and an `AVG` over a whole table are questions this can answer out of numbers it
//! already has rather than by reading twenty million rows, which is what `null_count`,
//! `exact_extremes` and `exact_sum` are for and what a native file has always done from its
//! directory. The load already paid for them, and [`MemoryTable::stats_ns`] is what it paid.
//!
//! The zone maps cannot say how many distinct values a column holds, which is the number every
//! cardinality estimate in the optimizer is built on, so there is a second thing built on the way in
//! next to them: one bottom-k sketch a column, over the whole table rather than per chunk, which is
//! `count.rs`. It is fixed size, so it costs the same 128 KB a column whether the table is a
//! thousand rows or a billion, and it is exact rather than estimated for any column with fewer distinct
//! values in it than the sketch has room for. [`MemoryTable::distinct_values`] is the exact half,
//! which a `COUNT(DISTINCT c)` may be read straight out of, and [`MemoryTable::distinct_estimate`]
//! is the one the estimator asks.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rudb_common::bounds::Bound;
use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::vector::VECTOR_SIZE;
use rudb_vector::{Builder, Chunk, Vector};

use crate::count::{Counts, Partial};
use crate::grams::Grams;
use crate::zone::{Probe, Range, Zone};

/// One column's statistics over one run of the chunks of an append: its count and a range a chunk.
type Counted = (Partial, Vec<Range>);

/// One column's [`MemoryTable::frequency_list`]: each value and how many rows hold it, or `None`
/// for a column with no list.
type Listed = Option<Arc<[(Value, u64)]>>;

/// How many rows one row group holds.
///
/// DuckDB's number, and what `spec/storage-v2/` is designed around, so a table in memory and a table
/// in a file are cut the same way and a checkpoint is not also a re-layout. It is exactly 120
/// [`VECTOR_SIZE`] chunks, which is not required by anything here and does mean a group filled by a
/// loader that hands over full chunks holds a whole number of them.
pub const ROWS_PER_GROUP: usize = 122_880;

/// One group's columns, each the full height of the group.
#[derive(Debug, Clone)]
struct Group {
    /// One page per column, in the table's column order.
    columns: Vec<Vector>,
    /// How many rows every one of those columns has.
    rows: usize,
    /// The chunks this group holds, in the numbering a scan reads.
    chunks: std::ops::Range<usize>,
    /// The zones of those chunks folded together, which is what rules the whole group out.
    zone: Zone,
}

/// Where one chunk's rows are.
///
/// Two cases and not one, because a group is laid end to end when it seals and a table being written
/// to has rows in it that no group has claimed yet. A reader in the middle of an insert sees those
/// rows as the chunks they arrived as, which is what the table did for every chunk before there were
/// groups, so it is a layout the read path already knows how to answer.
#[derive(Debug, Clone, Copy)]
enum Slot {
    /// `len` rows starting at row `at` of group `group`.
    Window { group: usize, at: usize, len: usize },
    /// A chunk of the group still filling, at position `at` of the open run.
    Open { at: usize },
    /// The small chunks at the end of the table, read as one chunk. Only ever the last slot.
    Tail,
}

/// The largest chunk the tail takes. Anything bigger is already a chunk worth its own slot.
const TAIL_TAKES: usize = 64;

/// How many chunks the tail holds before it lays them into one.
const TAIL_CHUNKS: usize = 32;

/// The most rows a chunk can have and still go into the columns being built rather than the tail.
const TAIL_BUILDS: usize = 8;

/// How many values an append counts statistics for on each thread it starts, at least.
const VALUES_PER_THREAD: usize = 1 << 16;

/// How many zones [`Zones`] keeps in one block.
const ZONE_BLOCK: usize = 256;

/// The zone of every chunk of a table, in blocks that copies of the table share until one of them
/// writes to a block.
///
/// A transaction takes a copy of every table it writes, and the zones were one `Vec`, so the copy
/// was a copy of the bounds of every column of every chunk, text bounds and all, which grows with
/// the table. Over a YCSB load of ten million rows in transactions of a thousand, that was five
/// thousand zones of eleven columns copied and freed per transaction by the end. Now a copy shares
/// the blocks, and a write copies the one block it writes to, which for an append is the last.
#[derive(Debug, Clone, Default)]
struct Zones {
    blocks: Vec<Arc<Vec<Zone>>>,
    len: usize,
}

impl Zones {
    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, zone: Zone) {
        if self.len % ZONE_BLOCK == 0 {
            self.blocks.push(Arc::new(Vec::with_capacity(ZONE_BLOCK)));
        }
        Arc::make_mut(self.blocks.last_mut().expect("a block for the zone")).push(zone);
        self.len += 1;
    }

    fn get(&self, at: usize) -> Option<&Zone> {
        self.blocks.get(at / ZONE_BLOCK)?.get(at % ZONE_BLOCK)
    }

    fn get_mut(&mut self, at: usize) -> Option<&mut Zone> {
        Arc::make_mut(self.blocks.get_mut(at / ZONE_BLOCK)?).get_mut(at % ZONE_BLOCK)
    }

    fn last(&self) -> Option<&Zone> {
        self.get(self.len.checked_sub(1)?)
    }

    fn last_mut(&mut self) -> Option<&mut Zone> {
        self.get_mut(self.len.checked_sub(1)?)
    }

    fn iter(&self) -> impl Iterator<Item = &Zone> {
        self.blocks.iter().flat_map(|block| block.iter())
    }
}

/// A table held in memory as row groups, read a chunk at a time.
#[derive(Debug, Clone)]
pub struct MemoryTable {
    types: Vec<LogicalType>,
    /// The sealed groups, one page per column each.
    groups: Vec<Group>,
    /// Where each chunk is, in the chunk numbering a scan reads.
    slots: Vec<Slot>,
    /// The chunks of the group still filling, in the order they arrived.
    open: Vec<Chunk>,
    /// How many rows are in `open`, which is what decides when it seals.
    open_rows: usize,
    /// The zones of the chunks in `open` folded together, so the group's zone is built as it fills
    /// rather than in a second pass at the seal.
    ///
    /// The tail is folded in when it closes and not a row at a time, so a trickled row widens the
    /// tail's zone and nothing else. [`Self::run_zone`] is the fold with the tail in it.
    open_zone: Option<Zone>,
    /// Small chunks that arrived one after another, held as one slot until they add up to a
    /// vector's worth of rows.
    ///
    /// A prepared `INSERT` of one row used to be a chunk and a slot of its own, and a table
    /// written a row at a time was then two hundred thousand one-row chunks that every scan walked
    /// one at a time: 858 ms for a `sum` and a `count(DISTINCT ...)` over 200,000 rows. The rows
    /// still arrive as small chunks and their statistics are still taken as they arrive, so the
    /// zone and the counts stay exact, but they sit behind one slot and are laid into one chunk
    /// when there are [`VECTOR_SIZE`] of them, or when a read asks for them.
    tail: Vec<Chunk>,
    /// The rows that arrived one at a time since the last chunk in `tail`, already in columns.
    ///
    /// They are the end of the tail. A small chunk that comes after them closes them into a chunk
    /// of their own first, so the order the rows arrived in is the order they are read in. Empty
    /// until the first row comes.
    ///
    /// Shared between copies of the table, as `counts` is, because a transaction takes a copy of
    /// every table when it starts, and copying a tail of a couple of thousand rows of text there
    /// was a twentieth of a keyed load in batches of a thousand rows. The copy is made by the
    /// first row written to it instead, and only by a transaction that writes one.
    building: Arc<Vec<Builder>>,
    /// How many rows are in `building`.
    built: usize,
    /// How many rows are in `tail` and `building` together.
    tail_rows: usize,
    /// One per chunk, in the same numbering as `slots`.
    zones: Zones,
    /// The distinct count of every column, over the whole table rather than per chunk.
    ///
    /// Per table and not per chunk because the question it answers is about the column, and a
    /// sketch a chunk is a sketch of a thousandth of the column that would have to be unioned with
    /// every other one to say anything. The sketch is fixed size, so one that sees every row costs
    /// the same as one that sees a chunk.
    counts: Arc<Counts>,
    /// The grams of every chunk of each string column, built the first time a `LIKE` asks about
    /// the column and dropped whenever a row is added. See [`crate::grams`].
    ///
    /// One per column, and a chunk whose rows could not all be read has `None`, which is a chunk
    /// that is always read.
    grams: Vec<OnceLock<Vec<Option<Grams>>>>,
    /// Each column's [`MemoryTable::frequencies`], built the first time the planner asks and dropped
    /// whenever a row is added, like `grams`. The planner asks for every column on every query, and
    /// building a list sorts it and copies each value out of the tally.
    lists: Vec<OnceLock<Listed>>,
    /// Each column's [`MemoryTable::exact_extremes`], worked out the first time they are asked for
    /// and dropped whenever a row is added, like `grams`. The compiler asks for them on every query
    /// and working them out walks the zone of every chunk.
    extremes: Vec<OnceLock<Option<(Bound, Bound)>>>,
    /// Each column's rows in groups coded into a dictionary, for [`MemoryTable::dictionary_rows`],
    /// counted the first time they are asked for and dropped whenever a row is added, like `grams`.
    /// The compiler asks for every column it scans on every query, and counting walks every group.
    coded: Vec<OnceLock<usize>>,
    rows: usize,
    stats_ns: u64,
    counts_ns: u64,
}

impl MemoryTable {
    /// An empty table of the given column types.
    #[must_use]
    pub fn new(types: Vec<LogicalType>) -> Self {
        let counts = Arc::new(Counts::new(types.len()));
        let grams = types.iter().map(|_| OnceLock::new()).collect();
        let lists = types.iter().map(|_| OnceLock::new()).collect();
        let extremes = types.iter().map(|_| OnceLock::new()).collect();
        let coded = types.iter().map(|_| OnceLock::new()).collect();
        Self {
            types,
            groups: Vec::new(),
            slots: Vec::new(),
            open: Vec::new(),
            open_rows: 0,
            open_zone: None,
            tail: Vec::new(),
            building: Arc::default(),
            built: 0,
            tail_rows: 0,
            zones: Zones::default(),
            counts,
            grams,
            lists,
            extremes,
            coded,
            rows: 0,
            stats_ns: 0,
            counts_ns: 0,
        }
    }

    /// The column types.
    #[must_use]
    pub fn types(&self) -> &[LogicalType] {
        &self.types
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.types.len()
    }

    /// How many rows, across every chunk.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether the table has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// How many chunks a scan will read.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.slots.len()
    }

    /// How many groups the rows are laid out in, which is what a scan does not see.
    ///
    /// Here so that a test can say what the layout is rather than guess at it from the read path,
    /// and so that anything measuring the table can report the number that actually moved.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Appends a chunk, which has to have the table's column types.
    ///
    /// An empty chunk is dropped rather than stored, because a scan that has to skip empty chunks
    /// is a scan with a branch in it that exists only because an operator upstream was sloppy.
    ///
    /// # Errors
    ///
    /// If the chunk's columns are not the table's columns.
    pub fn append(&mut self, chunk: Chunk) -> Result<()> {
        self.check(&chunk)?;
        self.forget_grams();
        if chunk.is_empty() {
            return Ok(());
        }
        let zone = self.take_stats(&chunk);
        let len = chunk.len();
        if len <= TAIL_TAKES {
            // A tail laid into one chunk has to fit in a vector, so a chunk that would take it past
            // one starts the next tail.
            if self.tail_rows + len > VECTOR_SIZE {
                self.close_tail()?;
            }
            // A row or a few go into the columns being built, the way a row handed over as values
            // does. Kept as a chunk each, they were laid into one every 32 chunks, so a table
            // written a row at a time through a key copied the whole tail every 32 rows, which was
            // most of what a keyed load cost.
            if len <= TAIL_BUILDS && chunk.kept().is_none() && self.build_chunk(&chunk) {
                return self.trail(zone, len);
            }
            // As pages, so that a read of a tail of one chunk shares it the way an open chunk is
            // shared.
            self.close_rows()?;
            self.tail.push(chunk.into_pages());
            self.trail(zone, len)?;
            if self.tail.len() >= TAIL_CHUNKS && self.built == 0 {
                // Laid into one chunk now and then, so the tail is a few chunks however small the
                // ones it took, and a copy of the table copies a few.
                let columns: Vec<usize> = (0..self.types.len()).collect();
                let laid = self.tail_read(&columns)?.into_pages();
                self.tail.clear();
                self.tail.push(laid);
            }
            return Ok(());
        }
        self.place(chunk, zone)
    }

    /// The zone of a chunk about to be kept, with the counts told about its rows.
    fn take_stats(&mut self, chunk: &Chunk) -> Zone {
        let started = Instant::now();
        let zone = Zone::of(chunk);
        let zoned = Instant::now();
        Arc::make_mut(&mut self.counts).add(chunk);
        self.counts_ns += zoned.elapsed().as_nanos() as u64;
        self.stats_ns += started.elapsed().as_nanos() as u64;
        zone
    }

    /// Counts `rows` that have just gone on the end of the tail, whose zone is `zone`.
    ///
    /// The tail's zone is the zones of what went into it folded together, which is what the zone of
    /// the one chunk they are read as would have been, and the counts have already seen the rows.
    fn trail(&mut self, zone: Zone, rows: usize) -> Result<()> {
        if self.tail_rows == 0 {
            self.slots.push(Slot::Tail);
            self.zones.push(zone);
        } else if let Some(last) = self.zones.last_mut() {
            last.widen(&zone);
        }
        self.lengthen(rows)
    }

    /// Counts `rows` onto the tail and closes it or the group when either is full.
    fn lengthen(&mut self, rows: usize) -> Result<()> {
        self.rows += rows;
        self.open_rows += rows;
        self.tail_rows += rows;
        if self.tail_rows >= VECTOR_SIZE {
            self.close_tail()?;
        }
        if self.open_rows >= ROWS_PER_GROUP {
            self.seal()?;
        }
        Ok(())
    }

    /// Makes the rows being built a chunk at the end of the tail.
    fn close_rows(&mut self) -> Result<()> {
        if self.built == 0 {
            return Ok(());
        }
        let columns = Arc::make_mut(&mut self.building)
            .iter_mut()
            .map(Builder::finish)
            .collect::<Result<Vec<_>>>()?;
        self.tail.push(Chunk::with_rows(columns, self.built)?.into_pages());
        self.built = 0;
        Ok(())
    }

    /// Lays the tail into one chunk and makes it an open chunk like any other.
    ///
    /// The slot stays where it is and so does its zone, so nothing a reader numbered moves.
    fn close_tail(&mut self) -> Result<()> {
        if self.tail_rows == 0 {
            return Ok(());
        }
        self.close_rows()?;
        if let Some(zone) = self.zones.last() {
            match &mut self.open_zone {
                Some(open) => open.widen(zone),
                None => self.open_zone = Some(zone.clone()),
            }
        }
        let chunk = if self.tail.len() == 1 {
            self.tail.pop().expect("one chunk")
        } else {
            let columns: Vec<usize> = (0..self.types.len()).collect();
            self.tail_read(&columns)?.into_pages()
        };
        if let Some(last) = self.slots.last_mut() {
            *last = Slot::Open { at: self.open.len() };
        }
        self.open.push(chunk);
        self.tail.clear();
        self.tail_rows = 0;
        Ok(())
    }

    /// Which chunk of the tail holds row `place` of it, and where in that chunk, once the rows
    /// being built are closed into one. `None` for a chunk that keeps only some of its rows.
    fn tail_place(&self, place: usize) -> Option<(usize, usize)> {
        let mut at = place;
        for (index, chunk) in self.tail.iter().enumerate() {
            if at < chunk.len() {
                return chunk.kept().is_none().then_some((index, at));
            }
            at -= chunk.len();
        }
        None
    }

    /// The named columns of the tail, laid end to end.
    ///
    /// The chunks are laid by the vector crate, with the rows being built as one more chunk after
    /// them. A column the vector crate will not lay, which the flat columns an `INSERT` builds never
    /// are, is put back together out of its values instead, so a read never fails over a layout.
    fn tail_read(&self, columns: &[usize]) -> Result<Chunk> {
        let mut picked = Vec::with_capacity(columns.len());
        for &column in columns {
            let ty = self.types.get(column).ok_or_else(|| {
                Error::internal(format!("column {column} of a table that has {}", self.types.len()))
            })?;
            let built = match self.building.get(column) {
                Some(builder) if self.built > 0 => Some(builder.vector()?),
                _ => None,
            };
            let mut pieces = Vec::with_capacity(self.tail.len() + 1);
            for chunk in &self.tail {
                pieces.push(chunk.column(column)?);
            }
            pieces.extend(built.as_ref());
            if let [only] = pieces.as_slice() {
                picked.push((*only).clone());
                continue;
            }
            if let Ok(Some(vector)) = rudb_vector::concat(ty, &pieces) {
                picked.push(vector);
                continue;
            }
            let mut values = Vec::with_capacity(self.tail_rows);
            for vector in &pieces {
                for row in 0..vector.len() {
                    values.push(vector.try_value_at(row)?);
                }
            }
            picked.push(Vector::from_values(ty.clone(), &values)?);
        }
        Chunk::with_rows(picked, self.tail_rows)
    }

    /// Appends every chunk of a finished result, with the statistics taken on up to `workers`
    /// threads.
    ///
    /// The table ends up the same as it would after [`Self::append`] a chunk at a time, in the same
    /// order. What differs is who does the statistics. The zone maps and the distinct counts are
    /// most of what an append costs, and each column is cut into a run of chunks per thread so that
    /// the work is many pieces of about the same size. Each run is counted into a [`Partial`] of its
    /// own and the runs are absorbed into the column in the order of their rows, which leaves the
    /// counts as reading every chunk in order would have.
    ///
    /// It used to be a column a thread, and on SF1 `lineitem` sorted that was 450ms for the comments
    /// and at most 100ms for any other column, so the append took as long as its one string column
    /// and most of the threads sat idle for most of it (#1380). The string columns are handed out
    /// first, because they are the ones that cost the most.
    ///
    /// The timings this keeps are the time each thread spent, added up, so they stay comparable
    /// with the ones [`Self::append`] keeps rather than shrinking with the thread count.
    ///
    /// # Errors
    ///
    /// If any chunk's columns are not the table's, in which case nothing is appended.
    pub fn append_all(&mut self, chunks: Vec<Chunk>, workers: usize) -> Result<()> {
        for chunk in &chunks {
            self.check(chunk)?;
        }
        // An append of a few rows, which an `INSERT ... VALUES` of one row is, goes on the tail as
        // a prepared insert's row does. As an open chunk of its own each was a slot and a zone, and
        // a table written a statement at a time was thousands of one-row chunks to scan and to copy
        // with every snapshot a transaction took.
        if chunks.iter().map(Chunk::len).sum::<usize>() <= TAIL_TAKES {
            for chunk in chunks {
                self.append(chunk)?;
            }
            return Ok(());
        }
        self.forget_grams();
        let chunks: Vec<Chunk> = chunks.into_iter().filter(|chunk| !chunk.is_empty()).collect();
        let per = chunks.len().div_ceil(workers.max(1)).max(1);
        let parts = chunks.len().div_ceil(per);
        let mut columns: Vec<usize> = (0..self.types.len()).collect();
        columns.sort_by_key(|&at| {
            !matches!(self.types.get(at), Some(LogicalType::Varchar | LogicalType::Blob))
        });
        let tasks: Vec<(usize, usize)> =
            columns.iter().flat_map(|&column| (0..parts).map(move |part| (column, part))).collect();
        // Each part turns away what the column's sketch would drop anyway, see [`Partial::under`].
        let ceilings: Vec<Option<u64>> = Arc::make_mut(&mut self.counts)
            .columns_mut()
            .iter()
            .map(|counting| counting.ceiling())
            .collect();
        let done: Vec<Mutex<Option<Counted>>> =
            (0..self.types.len() * parts).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let spent = AtomicU64::new(0);
        let counted = AtomicU64::new(0);
        let work = || {
            loop {
                let Some(&(column, part)) = tasks.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    return;
                };
                let run = chunks.get(part * per..chunks.len().min((part + 1) * per)).unwrap_or(&[]);
                let started = Instant::now();
                let mut counting_ns = 0;
                let mut partial = Partial::under(ceilings.get(column).copied().flatten());
                let mut ranges = Vec::with_capacity(run.len());
                for chunk in run {
                    let Ok(vector) = chunk.column(column) else { continue };
                    ranges.push(Range::of(vector));
                    let zoned = Instant::now();
                    partial.add(vector);
                    counting_ns += zoned.elapsed().as_nanos() as u64;
                }
                spent.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                counted.fetch_add(counting_ns, Ordering::Relaxed);
                let Some(slot) = done.get(column * parts + part) else { return };
                let Ok(mut slot) = slot.lock() else { return };
                *slot = Some((partial, ranges));
            }
        };
        // A thread is only worth starting for enough values to count. A single-row insert used to
        // start one per column and spent more on the starts than on the row.
        let values = chunks.iter().map(Chunk::len).sum::<usize>() * self.types.len();
        let threads = workers.clamp(1, tasks.len().max(1)).min(values.div_ceil(VALUES_PER_THREAD));
        if threads <= 1 {
            work();
        } else {
            std::thread::scope(|scope| {
                for _ in 1..threads {
                    scope.spawn(work);
                }
                work();
            });
        }
        let mut done = done.into_iter();
        let mut ranges = Vec::with_capacity(self.types.len());
        for mut counting in Arc::make_mut(&mut self.counts).columns_mut() {
            let mut taken = Vec::with_capacity(chunks.len());
            for slot in done.by_ref().take(parts) {
                let (partial, run) = slot
                    .into_inner()
                    .map_err(|_| {
                        Error::internal("a thread taking the statistics of an append panicked")
                    })?
                    .ok_or_else(|| Error::internal("a run of an append was not counted"))?;
                counting.absorb(partial);
                taken.extend(run);
            }
            if taken.len() != chunks.len() {
                return Err(Error::internal("a column of an append was not read to the end"));
            }
            ranges.push(taken.into_iter());
        }
        self.stats_ns += spent.into_inner();
        self.counts_ns += counted.into_inner();
        if !chunks.is_empty() {
            self.close_tail()?;
        }
        // The runs this fills are laid together, a column of a run a task, rather than as each
        // one fills. Laid as they filled, each took a thread per column, and the string column
        // was most of it, so a replay that put tens of millions of rows back spent seconds laying
        // strings on one thread with the other cores idle.
        let mut full = Vec::new();
        for chunk in chunks {
            let zone = Zone::from_ranges(ranges.iter_mut().filter_map(Iterator::next).collect());
            self.open_chunk(chunk, zone);
            if self.open_rows >= ROWS_PER_GROUP {
                full.push(self.take_open());
            }
        }
        let runs = full.iter().map(|run| run.chunks.as_slice()).collect::<Vec<_>>();
        let laid = lay_runs(&self.types, &runs, workers);
        for (run, columns) in full.into_iter().zip(laid) {
            self.settle(run, columns);
        }
        Ok(())
    }

    /// Refuses a chunk whose columns are not the table's.
    fn check(&self, chunk: &Chunk) -> Result<()> {
        if chunk.width() != self.types.len() {
            return Err(Error::internal(format!(
                "a chunk of {} columns appended to a table of {}",
                chunk.width(),
                self.types.len()
            )));
        }
        for (index, (held, wanted)) in chunk.types().iter().zip(&self.types).enumerate() {
            // A column of aggregate states is stored as the layout its states are written in, and
            // a vector of them is typed that way.
            if held != wanted.storage() {
                return Err(Error::internal(format!(
                    "column {index} of the chunk is {held} and the table's is {wanted}"
                )));
            }
        }
        Ok(())
    }

    /// Puts a chunk whose statistics have been taken into the group that is filling.
    fn place(&mut self, chunk: Chunk, zone: Zone) -> Result<()> {
        self.close_tail()?;
        self.open_chunk(chunk, zone);
        if self.open_rows >= ROWS_PER_GROUP {
            self.seal()?;
        }
        Ok(())
    }

    /// Adds a chunk to the open run, leaving sealing it to the caller, see [`Self::place`].
    fn open_chunk(&mut self, chunk: Chunk, zone: Zone) {
        match &mut self.open_zone {
            Some(open) => open.widen(&zone),
            None => self.open_zone = Some(zone.clone()),
        }
        self.rows += chunk.len();
        self.zones.push(zone);
        self.open_rows += chunk.len();
        self.slots.push(Slot::Open { at: self.open.len() });
        // Stored as pages even before the group seals, because a chunk that goes through the
        // fallback is never laid end to end and this is what makes reading it a reference count bump
        // rather than a copy. A page laid end to end afterwards is copied out of once, here.
        self.open.push(chunk.into_pages());
    }

    /// Lays the open chunks end to end into one group, or keeps them as the chunks they are.
    ///
    /// Every column has to lay for the group to lay, because a group holds one page per column of
    /// the same height and half of one is not a group. A column that will not lay is a column that
    /// arrived encoded, and encoded is the form worth keeping, so the answer is to leave the whole
    /// run as it came: each chunk becomes a group of its own, which is three numbers in the
    /// directory and is exactly what the table did before there were groups.
    ///
    /// Sealing is where the copy is. It is one pass over the rows of the group per column, and it
    /// buys every read afterwards a window into one run instead of a jump to one of a hundred and
    /// twenty allocations.
    fn seal(&mut self) -> Result<()> {
        self.close_tail()?;
        if self.open.is_empty() {
            return Ok(());
        }
        // The columns are laid on threads of their own once the group is a full one. Each is a
        // copy of every row into fresh memory, and one after the other they were most of the time
        // a replay of the log spent outside its decoders, with the other cores idle.
        let threads = if self.open_rows >= ROWS_PER_GROUP {
            std::thread::available_parallelism().map_or(1, usize::from)
        } else {
            1
        };
        // flatten: an `Option` of an `Option`, the one run asked for, and no column is copied.
        let laid = lay_runs(&self.types, &[self.open.as_slice()], threads).pop().flatten();
        let run = self.take_open();
        self.settle(run, laid);
        Ok(())
    }

    /// The open run, taken away to be sealed, which leaves no run open.
    fn take_open(&mut self) -> Run {
        let first = self.slots.len() - self.open.len();
        self.open_rows = 0;
        Run { first, chunks: std::mem::take(&mut self.open), zone: self.open_zone.take() }
    }

    /// Makes `run` a group of the pages `laid`, or a group per chunk when it would not lay.
    fn settle(&mut self, run: Run, laid: Option<Vec<Vector>>) {
        let Run { first, chunks, zone } = run;
        let last = first + chunks.len();
        match laid {
            Some(columns) => {
                let group = self.groups.len();
                let mut at = 0;
                for (slot, chunk) in self.slots[first..last].iter_mut().zip(&chunks) {
                    *slot = Slot::Window { group, at, len: chunk.len() };
                    at += chunk.len();
                }
                let zone = zone.unwrap_or_default();
                self.groups.push(Group { columns, rows: at, chunks: first..last, zone });
            }
            None => {
                // A group per chunk, so each one's zone is the chunk's own and the fold is thrown
                // away. It is the right answer rather than a shortcut: a group covering one chunk
                // that claimed the range of a hundred and twenty would rule out nothing.
                for (at, chunk) in (first..last).zip(chunks) {
                    let group = self.groups.len();
                    let rows = chunk.len();
                    self.slots[at] = Slot::Window { group, at: 0, len: rows };
                    let zone = self.zones.get(at).cloned().unwrap_or_default();
                    let columns = chunk.into_columns();
                    self.groups.push(Group { columns, rows, chunks: at..at + 1, zone });
                }
            }
        }
    }

    /// The chunks of each row group, in the numbering [`Self::read`] takes.
    ///
    /// One entry per group, and a last entry for the run still filling when there is one, because a
    /// scan started in the middle of an insert has to be able to reach those rows and they are not
    /// in any group yet. Together they cover every chunk exactly once and in order, which is what
    /// the scan above divides the work by.
    ///
    /// This is the same shape a native file answers from its directory, and it is answered here for
    /// the same reason: a morsel that covers a run of chunks is one a scan can walk ruled out chunks
    /// inside of, and a morsel that covers one chunk is drained the moment that chunk is ruled out.
    #[must_use]
    pub fn group_parts(&self) -> Vec<std::ops::Range<usize>> {
        let mut parts: Vec<std::ops::Range<usize>> =
            self.groups.iter().map(|group| group.chunks.clone()).collect();
        let open = self.open.len() + usize::from(self.tail_rows > 0);
        if open > 0 {
            parts.push((self.slots.len() - open)..self.slots.len());
        }
        parts
    }

    /// How many rows group `index` holds, in the numbering [`Self::group_parts`] hands back.
    ///
    /// A group knows its own height, so a scan dividing its work by rows does not have to add up a
    /// hundred and twenty chunk lengths to find out.
    #[must_use]
    pub fn group_rows(&self, index: usize) -> usize {
        match self.groups.get(index) {
            Some(group) => group.rows,
            // The open run, which is the entry `group_parts` puts after the groups.
            None if index == self.groups.len() => self.open_rows,
            None => 0,
        }
    }

    /// Whether the probes rule out every row of group `index`.
    ///
    /// The coarse level the note at the top of this file said was worth having. It is one test per
    /// hundred and twenty chunks, so it cannot be as selective as the chunk zones are, and what it
    /// buys is not selectivity: it is that a group ruled out here is a group whose chunks are never
    /// looked at, one at a time, to be ruled out again.
    ///
    /// A group with no zone is a group that is read, and so is an index past the end, for the same
    /// reason a chunk with no zone is read.
    #[must_use]
    pub fn group_skips(&self, index: usize, probes: &[Probe]) -> bool {
        match self.groups.get(index) {
            Some(group) => group.zone.skips(probes),
            // The open run, which is the entry `group_parts` puts after the groups.
            // Ruled out when the open chunks and the tail each are, which is never less than the
            // fold of the two would rule out.
            None if index == self.groups.len() => {
                let open = self.open_zone.as_ref();
                let tail = self.tail_zone();
                (open.is_some() || tail.is_some())
                    && open.is_none_or(|zone| zone.skips(probes))
                    && tail.is_none_or(|zone| zone.skips(probes))
            }
            None => false,
        }
    }

    /// The zone of the tail, when it holds rows.
    fn tail_zone(&self) -> Option<&Zone> {
        self.zones.last().filter(|_| self.tail_rows > 0)
    }

    /// The zone of the open run, the open chunks and the tail folded together.
    fn run_zone(&self) -> Option<Cow<'_, Zone>> {
        match (&self.open_zone, self.tail_zone()) {
            (Some(open), Some(tail)) => {
                let mut zone = open.clone();
                zone.widen(tail);
                Some(Cow::Owned(zone))
            }
            (open, tail) => open.as_ref().or(tail).map(Cow::Borrowed),
        }
    }

    /// The zone of group `index`, with the open run after the groups the way [`Self::group_skips`]
    /// numbers it.
    #[must_use]
    pub fn group_zone(&self, index: usize) -> Option<Cow<'_, Zone>> {
        match self.groups.get(index) {
            Some(group) => Some(Cow::Borrowed(&group.zone)),
            None if index == self.groups.len() => self.run_zone(),
            None => None,
        }
    }

    /// How long this table has spent building statistics, in nanoseconds.
    ///
    /// A load reports this next to its own wall time so that the price of the zone maps is a number
    /// somebody can argue with rather than something buried inside the load. See `zone.rs`.
    ///
    /// A row added on its own is not timed. Its statistics are a comparison and a hash a value, and
    /// the three clock reads it would take to time them cost more than that.
    #[must_use]
    pub fn stats_ns(&self) -> u64 {
        self.stats_ns
    }

    /// How much of [`MemoryTable::stats_ns`] went on the distinct counts, in nanoseconds.
    ///
    /// The two passes are timed apart because they are priced apart. A zone map is two comparisons
    /// a value and a distinct count is a hash a value, so one of them is worth several of the other
    /// and a single number would hide which one a slow load was paying for. See `count.rs`.
    #[must_use]
    pub fn counts_ns(&self) -> u64 {
        self.counts_ns
    }

    /// The zone of one chunk, or `None` past the end.
    #[must_use]
    pub fn zone(&self, index: usize) -> Option<&Zone> {
        self.zones.get(index)
    }

    /// How many distinct non-null values one column holds, when that number is exact.
    ///
    /// Exact means the sketch never filled up, so it is holding every distinct hash there was and
    /// counting them is counting the column. `None` otherwise, which is a column with at least
    /// [`rudb_encoding::sketch::DEFAULT_K`] distinct values in it or a column of a type `count.rs`
    /// has no rule for. This is the one a `COUNT(DISTINCT c)` may be answered out of.
    #[must_use]
    pub fn distinct_values(&self, column: usize) -> Option<u64> {
        self.counts.exact(column)
    }

    /// The same count, estimated where it is not exact, with a flag saying which it is.
    ///
    /// For the estimator, which would rather have a number at one and a half percent than the
    /// constant it uses when it has nothing. `None` is still `None`: a column this cannot count is
    /// a column that says so rather than one that guesses.
    #[must_use]
    pub fn distinct_estimate(&self, column: usize) -> Option<(u64, bool)> {
        self.counts.distinct(column)
    }

    /// Every value of one column with the exact number of rows holding it, most common first.
    ///
    /// `None` for a column with more distinct values than `tally.rs` counts, which is where the
    /// argument for the cap and for the list being whole or absent is. A list that is here is every
    /// value the column holds and every row is under one of them, so what does not appear in it holds
    /// no rows at all, and that is what makes it an answer rather than an estimate.
    ///
    /// The null is in the list when there is one, counted as a value of its own the way a `GROUP BY`
    /// makes it a group of its own, and it comes from the zone maps rather than from the tally. That
    /// is the one number the two halves of the statistics pass have to be put together for.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn frequencies(&self, column: usize) -> Result<Option<Vec<(Value, u64)>>> {
        let Some(mut held) = self.counts.frequencies(column) else { return Ok(None) };
        let nulls = self.null_count(column)? as u64;
        if nulls > 0 {
            // Put where its count belongs rather than on the end, because the list is read as most
            // common first and a column of mostly nulls has the null as its commonest value.
            let at = held.iter().position(|(_, rows)| *rows < nulls).unwrap_or(held.len());
            held.insert(at, (Value::Null, nulls));
        }
        Ok(Some(held))
    }

    /// [`MemoryTable::frequencies`] shared rather than copied, built once for as long as no row is
    /// added.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn frequency_list(&self, column: usize) -> Result<Listed> {
        let Some(list) = self.lists.get(column) else {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        };
        if let Some(held) = list.get() {
            return Ok(held.clone());
        }
        let held: Listed = self.frequencies(column)?.map(Arc::from);
        Ok(list.get_or_init(|| held).clone())
    }

    /// The smallest and the largest value of one string column, from the values its tally holds.
    ///
    /// Strings only, because they are the one type whose ends the zone maps can fail to give
    /// exactly. A string column that arrives as a dictionary narrower than its chunk gets ends read
    /// off the dictionary, which is a superset of what the rows point at, so
    /// [`MemoryTable::exact_extremes`] refuses it and a `MIN` over it walks every row. Numbers go
    /// through a gather that reads only the codes the rows use, so their ends are already exact.
    ///
    /// A null in the column is no obstacle here, unlike on the file side, where the placeholder a
    /// null is written as sorts ahead of every real value. Nothing null ever reaches the tally.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn text_extremes(&self, column: usize) -> Result<Option<(Value, Value)>> {
        let Some(ty) = self.types.get(column) else {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        };
        if !matches!(ty, LogicalType::Varchar | LogicalType::Blob) {
            return Ok(None);
        }
        Ok(self.counts.extremes(column))
    }

    /// How many distinct values one column's frequency list holds, without building it.
    ///
    /// For a caller deciding whether the list is worth copying. The null is not counted, so this is
    /// one short of [`MemoryTable::frequencies`] for a column that has one.
    #[must_use]
    pub fn frequency_values(&self, column: usize) -> Option<usize> {
        self.counts.frequency_values(column)
    }

    /// Whether the probes rule out every row of chunk `index`.
    ///
    /// A chunk with no zone is a chunk that is read, because saying nothing about a chunk has to
    /// mean keeping it. That is what makes this safe to ask about any index at all.
    #[must_use]
    pub fn skips(&self, index: usize, probes: &[Probe]) -> bool {
        self.zones.get(index).is_some_and(|zone| zone.skips(probes))
    }

    /// Whether no row of chunk `index` can hold every one of `needles` in the column it names.
    ///
    /// The needles are the pieces of the `LIKE` conjuncts of one filter, each with its column, so
    /// one of them missing rules the chunk out the way one probe does in [`Self::skips`]. The first
    /// question about a column builds its grams over every chunk, on up to `workers` threads, and
    /// every question after that reads them. A column that is not a string, and a chunk with no
    /// grams, rules nothing out.
    #[must_use]
    pub fn lacks(&self, index: usize, needles: &[(usize, Vec<u8>)], workers: usize) -> bool {
        needles.iter().any(|(column, needle)| {
            self.column_grams(*column, workers)
                .and_then(|grams| grams.get(index))
                .and_then(Option::as_ref)
                .is_some_and(|grams| grams.lacks(needle))
        })
    }

    /// The grams of every chunk of `column`, built on first use.
    fn column_grams(&self, column: usize, workers: usize) -> Option<&[Option<Grams>]> {
        if self.types.get(column) != Some(&LogicalType::Varchar) {
            return None;
        }
        let built = self.grams.get(column)?.get_or_init(|| {
            let count = self.chunk_count();
            let per = count.div_ceil(workers.max(1)).max(1);
            std::thread::scope(|scope| {
                let parts: Vec<_> = (0..count)
                    .step_by(per)
                    .map(|first| {
                        let past = (first + per).min(count);
                        let handle = scope.spawn(move || {
                            (first..past)
                                .map(|at| {
                                    let read = self.read(at, &[column]).ok()?;
                                    Grams::of(read.column(0).ok()?)
                                })
                                .collect::<Vec<_>>()
                        });
                        (past - first, handle)
                    })
                    .collect();
                // A part that failed still takes up its chunks, as chunks with no grams, so every
                // part after it lines up with the chunk numbers it was built for.
                parts
                    .into_iter()
                    .flat_map(|(len, handle)| handle.join().unwrap_or_else(|_| vec![None; len]))
                    .collect()
            })
        });
        Some(built)
    }

    /// Drops every column's grams and frequency list, because the rows are about to change under
    /// them.
    fn forget_grams(&mut self) {
        for grams in &mut self.grams {
            grams.take();
        }
        for list in &mut self.lists {
            list.take();
        }
        for ends in &mut self.extremes {
            ends.take();
        }
        for coded in &mut self.coded {
            coded.take();
        }
    }

    /// Whether the probes keep every row of chunk `index`.
    ///
    /// A chunk with no zone is a chunk that gets compared, for the same reason it is a chunk that
    /// gets read: saying nothing about a chunk has to mean doing the work on it.
    #[must_use]
    pub fn certain(&self, index: usize, probes: &[Probe]) -> bool {
        self.zones.get(index).is_some_and(|zone| zone.certain(probes))
    }

    /// How many rows of one column are null, added up over the chunks.
    ///
    /// Always an answer, because a zone's null count is the one number in it that never comes from a
    /// summary somebody else wrote: `Range::of` counts the validity mask whatever form the column
    /// arrived in. That is what makes it exact for a dictionary and a bit packed column too, where
    /// the two ends are allowed to be wider than the rows.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn null_count(&self, column: usize) -> Result<usize> {
        let mut nulls = 0;
        for (range, _) in self.ranges(column)? {
            nulls += range.nulls;
        }
        Ok(nulls)
    }

    /// The smallest and the largest value of one column, when every chunk walked its rows.
    ///
    /// A zone's ends are allowed to be wider than the truth, because ends that rule out a chunk that
    /// could not match are still right when they rule out nothing. That is what makes them cheap for
    /// a form this cannot read, and it is also what stops them answering a `MIN`, so each range says
    /// which of the two it is and this answers only when all of them looked.
    ///
    /// A chunk of nothing but nulls has no ends and says nothing about the column's, so it is
    /// skipped rather than given up on. A chunk that has rows and still has no ends is a form this
    /// cannot see into, and answering from the other chunks would answer with ends that do not cover
    /// its rows, so that one gives up.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn exact_extremes(&self, column: usize) -> Result<Option<(Bound, Bound)>> {
        let Some(ends) = self.extremes.get(column) else {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        };
        if let Some(held) = ends.get() {
            return Ok(held.clone());
        }
        let found = self.walk_extremes(column)?;
        Ok(ends.get_or_init(|| found).clone())
    }

    /// How many rows of one column are in sealed groups whose page for it is coded into a
    /// dictionary, and how many rows the table has, for a reader deciding whether to read the
    /// column through its codes.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn dictionary_rows(&self, column: usize) -> Result<(usize, usize)> {
        let Some(held) = self.coded.get(column) else {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        };
        let coded = *held.get_or_init(|| {
            self.groups
                .iter()
                .filter(|g| {
                    g.columns.get(column).is_some_and(|v| v.shared_dictionary_parts().is_some())
                })
                .map(|g| g.rows)
                .sum()
        });
        Ok((coded, self.len()))
    }

    /// [`MemoryTable::exact_extremes`] worked out from the zones.
    fn walk_extremes(&self, column: usize) -> Result<Option<(Bound, Bound)>> {
        let mut low: Option<Bound> = None;
        let mut high: Option<Bound> = None;
        for (range, rows) in self.ranges(column)? {
            if !range.exact {
                return Ok(None);
            }
            let (Some(small), Some(large)) = (range.low.as_ref(), range.high.as_ref()) else {
                if rows > range.nulls {
                    return Ok(None);
                }
                continue;
            };
            low = Some(low.map_or_else(|| small.clone(), |held| held.smaller(small.clone())));
            high = Some(high.map_or_else(|| large.clone(), |held| held.larger(large.clone())));
        }
        Ok(low.zip(high))
    }

    /// The total of one integer column and how many rows went into it, when every chunk has a total.
    ///
    /// The count beside the total is the rows that are not null, because that is what a `SUM` adds up
    /// and what an `AVG` divides by, and working it out from the row count and the null count
    /// afterwards would walk the same zones twice.
    ///
    /// `None` for a column no chunk of which could be added up, which is every column that is not an
    /// integer one, and for a table so large that adding the chunks together overflows an `i128`.
    ///
    /// # Errors
    ///
    /// If the column is outside the table.
    pub fn exact_sum(&self, column: usize) -> Result<Option<(i128, u64)>> {
        let mut total = 0_i128;
        let mut rows = 0_u64;
        for (range, held) in self.ranges(column)? {
            let Some(part) = range.sum else { return Ok(None) };
            let Some(sum) = total.checked_add(part) else { return Ok(None) };
            total = sum;
            rows = rows.saturating_add((held - range.nulls) as u64);
        }
        Ok(Some((total, rows)))
    }

    /// The range of one column of every chunk, beside how many rows that chunk has.
    ///
    /// Collected rather than returned as an iterator so that a zone narrower than the table is an
    /// error here instead of a chunk quietly dropped out of the middle of a fold, which would answer
    /// a total over some of the rows as though it were over all of them.
    ///
    /// # Errors
    ///
    /// If the column is outside the table, or if a chunk has no range for it, which `append` makes
    /// impossible by building the zone from the chunk it has already checked the width of.
    fn ranges(&self, column: usize) -> Result<Vec<(&Range, usize)>> {
        if column >= self.types.len() {
            return Err(Error::internal(format!(
                "column {column} of a table that has {}",
                self.types.len()
            )));
        }
        let mut found = Vec::with_capacity(self.zones.len());
        for (zone, slot) in self.zones.iter().zip(&self.slots) {
            let range = zone
                .column(column)
                .ok_or_else(|| Error::internal("a chunk's zone is narrower than the table"))?;
            found.push((range, self.rows_of(*slot)));
        }
        Ok(found)
    }

    /// How many rows one slot names.
    fn rows_of(&self, slot: Slot) -> usize {
        match slot {
            Slot::Window { len, .. } => len,
            Slot::Open { at } => self.open.get(at).map_or(0, Chunk::len),
            Slot::Tail => self.tail_rows,
        }
    }

    /// Appends rows given one at a time, splitting them into chunks.
    ///
    /// The slow way in, for an `INSERT` and for a test. It transposes, which is the whole cost:
    /// rows arrive across the columns and a chunk is down them.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, or if a value is not one its column can hold.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        self.forget_grams();
        for (index, row) in rows.iter().enumerate() {
            if row.len() != self.types.len() {
                return Err(Error::internal(format!(
                    "row {index} has {} values and the table has {} columns",
                    row.len(),
                    self.types.len()
                )));
            }
        }
        if let [row] = rows {
            return self.push_row(row);
        }
        for batch in rows.chunks(VECTOR_SIZE) {
            let mut columns = Vec::with_capacity(self.types.len());
            for (position, ty) in self.types.iter().enumerate() {
                let down: Vec<Value> = batch.iter().map(|row| row[position].clone()).collect();
                columns.push(Vector::from_values(ty.clone(), &down)?);
            }
            self.append(Chunk::with_rows(columns, batch.len())?)?;
        }
        Ok(())
    }

    /// Appends one row, the same as [`Self::append_rows`] with that one row.
    ///
    /// # Errors
    ///
    /// If the row is not as wide as the table, or if a value is not one its column can hold.
    pub fn append_row(&mut self, row: &[Value]) -> Result<()> {
        self.forget_grams();
        if row.len() != self.types.len() {
            return Err(Error::internal(format!(
                "row 0 has {} values and the table has {} columns",
                row.len(),
                self.types.len()
            )));
        }
        self.push_row(row)
    }

    /// Puts one row of the right width on the end of the tail, into the columns being built.
    ///
    /// The statistics are the ones a one-row chunk of it would have, the same chunk
    /// [`Self::append`] would have been handed. For a row of plain values they are worked out from
    /// the values, and anything else is built into the chunk and taken from that.
    fn push_row(&mut self, row: &[Value]) -> Result<()> {
        // A row of plain values has its zone and counts worked out from the values, which is what
        // the one-row chunk would have given them without the chunk.
        // Past the tail's first row the tail's zone is widened in place from the values.
        if self.tail_rows > 0 && Zone::takes_row(row, &self.types) {
            self.build(row)?;
            Arc::make_mut(&mut self.counts).add_row(row);
            if let Some(last) = self.zones.last_mut() {
                last.widen_row(row, &self.types);
            }
            return self.lengthen(1);
        }
        if let Some(zone) = Zone::of_row(row, &self.types) {
            self.build(row)?;
            Arc::make_mut(&mut self.counts).add_row(row);
            return self.trail(zone, 1);
        }
        let mut columns = Vec::with_capacity(self.types.len());
        for (value, ty) in row.iter().zip(&self.types) {
            columns.push(Vector::from_values(ty.clone(), std::slice::from_ref(value))?);
        }
        let chunk = Chunk::with_rows(columns, 1)?;
        self.check(&chunk)?;
        self.build(row)?;
        let zone = self.take_stats(&chunk);
        self.trail(zone, 1)
    }

    /// Pushes every row of `chunk` into the columns being built, and says whether it did. A row
    /// that will not go in takes the ones before it back out, and the chunk is left to be kept as
    /// it is.
    ///
    /// A chunk with codes into a table wide dictionary is kept as it is too. Built into the columns
    /// its codes would turn back into strings, and a read that gathers the column hands on the
    /// codes only when every piece of it is still codes into the one dictionary.
    fn build_chunk(&mut self, chunk: &Chunk) -> bool {
        if chunk.columns().iter().any(|column| column.stable_dictionary_parts().is_some()) {
            return false;
        }
        let start = self.built;
        for row in 0..chunk.len() {
            let values = (0..chunk.width())
                .map(|column| chunk.try_value_at(row, column))
                .collect::<Result<Vec<_>>>();
            if values.and_then(|values| self.build(&values)).is_err() {
                for builder in Arc::make_mut(&mut self.building) {
                    builder.truncate(start);
                }
                self.built = start;
                return false;
            }
        }
        true
    }

    /// Pushes one row into the columns being built, all of it or none of it.
    fn build(&mut self, row: &[Value]) -> Result<()> {
        let building = Arc::make_mut(&mut self.building);
        if building.is_empty() {
            *building = self.types.iter().map(|ty| Builder::new(ty.clone(), VECTOR_SIZE)).collect();
        }
        for (at, value) in row.iter().enumerate() {
            if let Err(error) = building[at].push(value) {
                for builder in &mut building[..at] {
                    builder.truncate(self.built);
                }
                return Err(error);
            }
        }
        self.built += 1;
        Ok(())
    }

    /// One chunk's worth of the named columns, in the order they are named.
    ///
    /// The columns are shared rather than copied. A chunk is a window into its group's pages, so the
    /// vector handed back here points at the stored values and the cost of this call is one atomic
    /// increment per column and whatever the cut has to rewrite. It used to copy, and
    /// `spec/perf/12-the-chunk-and-the-page.md` measured
    /// what that cost: 1,803 instructions a chunk of memcpy and 1,869 of malloc and free on a `sum`
    /// over a twenty million row table, which is 27 percent of the chunk and none of it the query's
    /// work.
    ///
    /// Nothing downstream can tell, because a write through a shared buffer copies it out first,
    /// which is `Buffer::to_mut`, so an operator that means to modify a column it was handed pays
    /// the copy there instead and one that does not pays nothing. Asking for the columns rather than
    /// taking them all still matters, because the atomic increments and the validity masks are per
    /// column, and it is the same reason projection pushdown exists.
    ///
    /// What is still copied is the parts a cut has to rewrite: a string column's views, a
    /// dictionary's codes, and a validity mask that is not all valid or all invalid. Those are
    /// sixteen, four and an eighth of a byte a row against the payload, and each of them is a `Vec`
    /// where the payload is a page.
    ///
    /// # Errors
    ///
    /// If there is no such chunk, or if a column is past the end of the table.
    pub fn read(&self, chunk: usize, columns: &[usize]) -> Result<Chunk> {
        let slot = *self.slots.get(chunk).ok_or_else(|| {
            Error::internal(format!(
                "chunk {chunk} of a table that has {} chunks",
                self.slots.len()
            ))
        })?;
        match slot {
            Slot::Window { group, at, len } => {
                let held = self
                    .groups
                    .get(group)
                    .ok_or_else(|| Error::internal("a chunk names a group that is not there"))?;
                // Checked once here rather than once per column, because a window past the end of
                // its group is the directory disagreeing with the pages and the message wanted is
                // that, not the same out of range cut reported by whichever column was read first.
                if at + len > held.rows {
                    return Err(Error::internal(format!(
                        "chunk {chunk} is rows {at} to {} of a group of {}",
                        at + len,
                        held.rows
                    )));
                }
                let mut picked = Vec::with_capacity(columns.len());
                for &column in columns {
                    let page = held.columns.get(column).ok_or_else(|| {
                        Error::internal(format!(
                            "column {column} of a table that has {}",
                            self.types.len()
                        ))
                    })?;
                    picked.push(page.slice(at, len)?);
                }
                Chunk::with_rows(picked, len)
            }
            Slot::Open { at } => {
                let held = self
                    .open
                    .get(at)
                    .ok_or_else(|| Error::internal("a chunk names an open chunk that is gone"))?;
                let mut picked = Vec::with_capacity(columns.len());
                for &column in columns {
                    picked.push(held.column(column)?.clone());
                }
                Chunk::with_rows(picked, held.len())
            }
            Slot::Tail => self.tail_read(columns),
        }
    }

    /// The named columns of chunk `chunk` at the rows `positions` names, which rise.
    ///
    /// What [`Self::read`] and a gather would give, without the chunk being cut or laid first. A
    /// window of a group is a cut that copies a string column's views, and the tail is laid end to
    /// end before it is read, so a lookup of one row by its key copied every row of its chunk in
    /// every column it read, which on a table of ten text columns was most of what a lookup cost.
    /// Here the rows are gathered straight out of the page, the chunk or the columns being built
    /// they are in, and a tail read whose rows are not all in one of its pieces is read whole.
    ///
    /// # Errors
    ///
    /// As [`Self::read`], and if a position is past the end of the chunk.
    pub fn read_rows(&self, chunk: usize, columns: &[usize], positions: &[u32]) -> Result<Chunk> {
        let slot = *self.slots.get(chunk).ok_or_else(|| {
            Error::internal(format!(
                "chunk {chunk} of a table that has {} chunks",
                self.slots.len()
            ))
        })?;
        let len = self.rows_of(slot);
        if positions.last().is_some_and(|&last| last as usize >= len) {
            return Err(Error::internal(format!("a row past the end of chunk {chunk}")));
        }
        fn gathered<'v>(
            pick: &dyn Fn(usize) -> Result<Cow<'v, Vector>>,
            columns: &[usize],
            at: &[u32],
        ) -> Result<Chunk> {
            let mut picked = Vec::with_capacity(columns.len());
            for &column in columns {
                picked.push(pick(column)?.gather(at)?);
            }
            Chunk::with_rows(picked, at.len())
        }
        fn column_of(chunk: &Chunk, column: usize) -> Result<Cow<'_, Vector>> {
            chunk.column(column).map(Cow::Borrowed)
        }
        match slot {
            Slot::Window { group, at, .. } => {
                let held = self
                    .groups
                    .get(group)
                    .ok_or_else(|| Error::internal("a chunk names a group that is not there"))?;
                let shifted: Vec<u32> =
                    positions.iter().map(|&position| position + at as u32).collect();
                let page = |column: usize| {
                    held.columns.get(column).map(Cow::Borrowed).ok_or_else(|| {
                        Error::internal(format!(
                            "column {column} of a table that has {}",
                            self.types.len()
                        ))
                    })
                };
                gathered(&page, columns, &shifted)
            }
            Slot::Open { at } => {
                let held = self
                    .open
                    .get(at)
                    .ok_or_else(|| Error::internal("a chunk names an open chunk that is gone"))?;
                gathered(&|column| column_of(held, column), columns, positions)
            }
            Slot::Tail => {
                if let (Some(&first), Some(&last)) = (positions.first(), positions.last()) {
                    let (first, last) = (first as usize, last as usize);
                    let mut start = 0;
                    let mut pieces = self.tail.iter();
                    let piece = loop {
                        match pieces.next() {
                            Some(piece) if first >= start + piece.len() => start += piece.len(),
                            other => break other,
                        }
                    };
                    let local: Vec<u32> =
                        positions.iter().map(|&position| position - start as u32).collect();
                    match piece {
                        Some(piece) if last < start + piece.len() && piece.kept().is_none() => {
                            return gathered(&|column| column_of(piece, column), columns, &local);
                        }
                        None if self.built > 0 => {
                            let mut picked = Vec::with_capacity(columns.len());
                            for &column in columns {
                                let builder = self.building.get(column).ok_or_else(|| {
                                    Error::internal("a column past the end of the table")
                                })?;
                                picked.push(builder.gather(&local)?);
                            }
                            return Chunk::with_rows(picked, positions.len());
                        }
                        _ => {}
                    }
                }
                let whole = self.tail_read(columns)?;
                let mut picked = Vec::with_capacity(columns.len());
                for column in 0..whole.width() {
                    picked.push(whole.column(column)?.gather(positions)?);
                }
                Chunk::with_rows(picked, positions.len())
            }
        }
    }

    /// How many rows chunk `index` has, or `None` past the end.
    ///
    /// Answered out of the directory, which is why it is here rather than left to a caller that
    /// reads the chunk and asks how long it is. A caller walking the table to turn a row ordinal
    /// into a chunk and an offset wants the lengths and not the rows, and reading every column of
    /// every chunk to find out how many rows are in them is the cost this is for.
    #[must_use]
    pub fn chunk_len(&self, index: usize) -> Option<usize> {
        self.slots.get(index).map(|&slot| self.rows_of(slot))
    }

    /// One stored chunk, whole.
    ///
    /// Every column of it, which for a sealed group is a window per column and for an open one is a
    /// reference count bump per column. It comes back owned rather than borrowed because a chunk is
    /// something the table now builds when it is asked for rather than something it holds, and that
    /// is the whole point of the groups: the rows of a chunk are next to the rows of its neighbours
    /// instead of in an allocation of their own.
    #[must_use]
    pub fn chunk(&self, index: usize) -> Option<Chunk> {
        let columns: Vec<usize> = (0..self.types.len()).collect();
        self.read(index, &columns).ok()
    }
}

impl MemoryTable {
    /// Writes `values` over the columns `targets` of row `place` of chunk `chunk`, which is how an
    /// `UPDATE` of one row lands without the table being built again, and says whether it could.
    ///
    /// A row of the tail is written in the small chunk it went in with, since the tail is laid out
    /// again from those whenever it is read. The rows still being built are closed into a chunk
    /// first, so a row the table took a moment ago can be written too. A column of a sealed group
    /// is written where it is, see [`Vector::put`], so the cost is the row unless somebody still
    /// holds the page.
    ///
    /// The zones of the chunk and of the run it is in take the new values and stop saying their
    /// ends and totals are exact, the counts take them as [`Counts::rewrite`] says, and what was
    /// worked out from the rows is dropped the way an append drops it.
    ///
    /// # Errors
    ///
    /// If there is no such row or column, or a value is not one its column holds.
    ///
    /// # Panics
    ///
    /// Never: a row found in the tail has its place in the tail.
    pub fn put_row(
        &mut self,
        chunk: usize,
        place: usize,
        targets: &[usize],
        values: &[Value],
    ) -> Result<bool> {
        let slot = *self
            .slots
            .get(chunk)
            .ok_or_else(|| Error::internal("a row written in a chunk that is not there"))?;
        if place >= self.rows_of(slot) {
            return Err(Error::internal("a row written past the end of its chunk"));
        }
        if targets.len() != values.len() {
            return Err(Error::internal("a row written with a value short or over"));
        }
        for (&column, value) in targets.iter().zip(values) {
            let ty = self
                .types
                .get(column)
                .ok_or_else(|| Error::internal("a row written in a column past the table"))?;
            if !value.is_null() && !value.is_of(ty) {
                return Err(Error::internal("a row written with a value of another type"));
            }
        }
        match slot {
            Slot::Window { group, .. } if group >= self.groups.len() => {
                return Err(Error::internal("a chunk names a group that is not there"));
            }
            Slot::Open { at } if self.open.get(at).is_none_or(|held| held.kept().is_some()) => {
                return Ok(false);
            }
            _ => {}
        }
        let tail = match slot {
            Slot::Tail => {
                self.close_rows()?;
                let Some(found) = self.tail_place(place) else { return Ok(false) };
                Some(found)
            }
            _ => None,
        };
        self.forget_grams();
        for (&column, value) in targets.iter().zip(values) {
            let was_null = match slot {
                Slot::Window { group, at, .. } => {
                    let held = &mut self.groups[group];
                    let page = &mut held.columns[column];
                    let was_null = page.try_value_at(at + place)?.is_null();
                    page.put(at + place, value)?;
                    held.zone.rewrite(column, was_null, value, &self.types[column]);
                    was_null
                }
                Slot::Open { at } => {
                    let held = std::mem::replace(&mut self.open[at], Chunk::empty(&[]));
                    let rows = held.len();
                    let mut columns = held.into_columns();
                    let was_null = columns[column].try_value_at(place)?.is_null();
                    let put = columns[column].put(place, value);
                    self.open[at] = Chunk::with_rows(columns, rows)?;
                    put?;
                    if let Some(zone) = &mut self.open_zone {
                        zone.rewrite(column, was_null, value, &self.types[column]);
                    }
                    was_null
                }
                Slot::Tail => {
                    let (index, at) = tail.expect("found above for a row of the tail");
                    let held = std::mem::replace(&mut self.tail[index], Chunk::empty(&[]));
                    let rows = held.len();
                    let mut columns = held.into_columns();
                    let was_null = columns[column].try_value_at(at)?.is_null();
                    let put = columns[column].put(at, value);
                    self.tail[index] = Chunk::with_rows(columns, rows)?;
                    put?;
                    was_null
                }
            };
            if let Some(zone) = self.zones.get_mut(chunk) {
                zone.rewrite(column, was_null, value, &self.types[column]);
            }
            Arc::make_mut(&mut self.counts).rewrite(column, value);
        }
        Ok(true)
    }
}

/// An open run taken away to be sealed: the slot of its first chunk, its chunks, and its zone.
struct Run {
    first: usize,
    chunks: Vec<Chunk>,
    zone: Option<Zone>,
}

/// Each of `runs` as one page per column of `types`, or `None` for a run with a column that will
/// not lay end to end, a column of a run a task on up to `threads` threads.
///
/// Collected a column at a time rather than a chunk at a time because that is the direction the
/// pages run in, and the borrow of each chunk's column ends inside the loop, so nothing is cloned
/// to build the list handed to [`rudb_vector::concat()`].
///
/// An error from the concatenation is treated as a run that will not lay. It means a piece said it
/// was flat and did not hold a flat run of its own length, which the vector crate believes is
/// impossible, and the safe thing for a table to do about a layout it cannot build is to keep the
/// rows it was given rather than to fail an insert over it.
fn lay_runs(types: &[LogicalType], runs: &[&[Chunk]], threads: usize) -> Vec<Option<Vec<Vector>>> {
    let lay = |run: &[Chunk], column: usize| -> Option<Vector> {
        let pieces = run
            .iter()
            .map(|chunk| chunk.column(column).ok().cloned())
            .collect::<Option<Vec<_>>>()?;
        rudb_vector::concat(types.get(column)?, &pieces).ok()?
    };
    let width = types.len();
    let tasks = runs.len() * width;
    let threads = threads.clamp(1, tasks.max(1));
    if threads < 2 {
        return runs
            .iter()
            .map(|run| (0..width).map(|column| lay(run, column)).collect())
            .collect();
    }
    let next = AtomicUsize::new(0);
    let laid: Vec<Mutex<Option<Vector>>> = (0..tasks).map(|_| Mutex::new(None)).collect();
    let work = || {
        loop {
            let task = next.fetch_add(1, Ordering::Relaxed);
            let (Some(run), Some(slot)) = (runs.get(task / width), laid.get(task)) else {
                return;
            };
            let page = lay(run, task % width);
            if let Ok(mut slot) = slot.lock() {
                *slot = page;
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            scope.spawn(work);
        }
        work();
    });
    let mut laid = laid.into_iter().map(|slot| slot.into_inner().ok().flatten());
    // Taken a run at a time before any is judged, because a column that would not lay stops a
    // collect into an `Option` short of the run's end.
    runs.iter()
        .map(|_| laid.by_ref().take(width).collect::<Vec<_>>().into_iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_of_a_table_writes_its_rows_being_built_and_counts_apart_from_the_original() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let row = |at: i64| vec![Value::BigInt(at), Value::Varchar(format!("value {at}"))];
        let mut table = MemoryTable::new(types);
        for at in 0..10 {
            table.append_row(&row(at)).expect("a row of the table's type");
        }
        let mut copy = table.clone();
        assert!(Arc::ptr_eq(&copy.building, &table.building), "the copy shares the rows");
        assert!(Arc::ptr_eq(&copy.counts, &table.counts), "the copy shares the counts");
        for at in 10..15 {
            copy.append_row(&row(at)).expect("a row of the table's type");
        }
        table.append_row(&row(99)).expect("a row of the table's type");
        let read = |table: &MemoryTable| {
            (0..table.chunk_count())
                .flat_map(|chunk| {
                    let chunk = table.read(chunk, &[0, 1]).expect("a chunk");
                    (0..chunk.len()).map(move |at| chunk.value_at(at, 0)).collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(read(&copy), (0..15).map(Value::BigInt).collect::<Vec<_>>());
        let mut kept = (0..10).map(Value::BigInt).collect::<Vec<_>>();
        kept.push(Value::BigInt(99));
        assert_eq!(read(&table), kept);
        assert_eq!(copy.distinct_values(0), Some(15));
        assert_eq!(table.distinct_values(0), Some(11));
    }

    /// A copy of a table shares the blocks of its zones, and a chunk appended to the copy, or a
    /// zone widened in it, leaves the original's zones as they were.
    #[test]
    fn a_copy_of_a_table_shares_its_zones_until_it_writes_one() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let chunk = |from: i64| {
            let ints = (from..from + 100).map(Value::BigInt).collect::<Vec<_>>();
            let texts = (from..from + 100).map(|at| Value::Varchar(format!("v{at}")));
            Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &ints).expect("a column"),
                Vector::from_values(LogicalType::Varchar, &texts.collect::<Vec<_>>())
                    .expect("a column"),
            ])
            .expect("a chunk")
        };
        let mut table = MemoryTable::new(types);
        for at in 0..600 {
            table.append(chunk(at * 100)).expect("a chunk of the table's types");
        }
        assert_eq!(table.zones.len(), table.chunk_count());
        let before = (0..table.chunk_count())
            .map(|at| table.zone(at).cloned().expect("a zone per chunk"))
            .collect::<Vec<_>>();
        let mut copy = table.clone();
        for (copied, kept) in copy.zones.blocks.iter().zip(&table.zones.blocks) {
            assert!(Arc::ptr_eq(copied, kept), "the copy shares every block");
        }
        copy.append(chunk(1_000_000)).expect("a chunk of the table's types");
        copy.append_row(&[Value::BigInt(-5), Value::Varchar("a".to_string())]).expect("a row");
        assert!(Arc::ptr_eq(&copy.zones.blocks[0], &table.zones.blocks[0]), "an untouched block");
        assert_eq!(copy.zones.len(), copy.chunk_count());
        assert_eq!(table.chunk_count(), before.len());
        for (at, zone) in before.iter().enumerate() {
            assert_eq!(table.zone(at), Some(zone), "zone {at} of the original");
        }
        let zones = copy.zones.iter().cloned().collect::<Vec<_>>();
        assert_eq!(zones[..before.len()], before[..]);
        assert_eq!(zones.len(), copy.chunk_count());
    }

    #[test]
    fn a_chunk_whose_grams_lack_a_word_is_ruled_out_until_rows_arrive() {
        let chunk = |text: &str| {
            let text = Value::Varchar(text.to_string());
            Chunk::new(vec![Vector::constant(LogicalType::Varchar, text, VECTOR_SIZE)])
                .expect("one column")
        };
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        table.append(chunk("http://example.com/")).expect("a chunk of the table's type");
        table.append(chunk("http://google.com/")).expect("a chunk of the table's type");
        let google = [(0, b"google".to_vec())];
        assert!(table.lacks(0, &google, 2));
        assert!(!table.lacks(1, &google, 2));
        table.append(chunk("https://google.ru/")).expect("a chunk of the table's type");
        assert!(!table.lacks(2, &google, 2), "the grams were not built again for the new chunk");
        assert!(table.lacks(0, &google, 2));
        assert!(!table.lacks(0, &[(1, b"google".to_vec())], 2), "a column past the end");
    }

    fn people() -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Varchar]);
        table
            .append_rows(&[
                vec![Value::Integer(1), Value::Varchar("ada".to_string())],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Integer(3), Value::Varchar("grace".to_string())],
            ])
            .expect("three rows of the table's own types");
        table
    }

    /// The case the zone maps are not allowed to answer. Four values behind five rows, so
    /// `zone::stringy` reads the dictionary rather than the rows and gets ends that are wider than
    /// the column: nothing points at "quince". The tally has only what the rows hold.
    #[test]
    fn the_ends_of_a_dictionary_string_column_come_from_the_tally_and_not_from_the_zones() {
        let values: Vec<Value> = ["kiwi", "apple", "pear", "quince"]
            .iter()
            .map(|held| Value::Varchar((*held).to_string()))
            .collect();
        let inner = Vector::from_values(LogicalType::Varchar, &values).expect("a dictionary");
        let vector = Vector::dictionary(vec![1, 2, 1, 2, 1], inner).expect("five rows");
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        table.append(Chunk::new(vec![vector]).expect("a chunk")).expect("the table's own type");
        assert_eq!(table.exact_extremes(0).expect("the only column"), None, "wider than the rows");
        assert_eq!(
            table.text_extremes(0).expect("the only column"),
            Some((Value::Varchar("apple".into()), Value::Varchar("pear".into()))),
            "not kiwi and not quince, which are in the dictionary and in no row"
        );
    }

    #[test]
    fn a_column_that_is_not_a_string_has_no_text_ends_and_a_missing_one_is_an_error() {
        // A number's zone maps are exact already, because a coded one is gathered through its codes
        // rather than read off its values, so there is nothing here for this to add.
        let table = people();
        assert_eq!(table.text_extremes(0).expect("the integer column"), None);
        assert_eq!(
            table.text_extremes(1).expect("the string column"),
            Some((Value::Varchar("ada".into()), Value::Varchar("grace".into()))),
            "the null between them is not an end and is not counted as one"
        );
        assert!(table.text_extremes(2).is_err());
    }

    #[test]
    fn a_load_reports_what_the_statistics_cost_and_which_half_of_it_was_the_counts() {
        let table = people();
        assert!(table.stats_ns() > 0, "three rows took no measurable time at all");
        assert!(table.counts_ns() > 0, "the sketches took no measurable time at all");
        assert!(table.counts_ns() <= table.stats_ns(), "a part is larger than the whole");
    }

    /// A table of one integer column over several chunks, so the folds have something to fold.
    fn counted(rows: usize) -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        let values: Vec<Vec<Value>> = (0..rows)
            .map(|row| {
                vec![if row % 5 == 0 { Value::Null } else { Value::Integer(row as i32 % 7) }]
            })
            .collect();
        table.append_rows(&values).expect("one integer a row");
        table
    }

    #[test]
    fn a_shared_frequency_list_is_built_again_after_rows_are_added() {
        let mut table = counted(100);
        let first = table.frequency_list(0).expect("a column").expect("a list");
        assert_eq!(first.to_vec(), table.frequencies(0).expect("a column").expect("a list"));
        let again = table.frequency_list(0).expect("a column").expect("a list");
        assert!(Arc::ptr_eq(&first, &again), "the list was built twice for the same rows");
        table.append_row(&[Value::Integer(100)]).expect("one integer");
        let after = table.frequency_list(0).expect("a column").expect("a list");
        assert_eq!(after.to_vec(), table.frequencies(0).expect("a column").expect("a list"));
        assert_eq!(after.len(), first.len() + 1);
        assert!(table.frequency_list(1).is_err());
    }

    #[test]
    fn kept_extremes_are_worked_out_again_after_rows_are_added() {
        let mut table = counted(VECTOR_SIZE * 2 + 10);
        let ends = Some((Bound::Int(0), Bound::Int(6)));
        assert_eq!(table.exact_extremes(0).expect("a column"), ends);
        assert_eq!(table.exact_extremes(0).expect("a column"), table.walk_extremes(0).unwrap());
        table.append_row(&[Value::Integer(-40)]).expect("one integer");
        let ends = Some((Bound::Int(-40), Bound::Int(6)));
        assert_eq!(table.exact_extremes(0).expect("a column"), ends);
        table.append_rows(&[vec![Value::Integer(90)]]).expect("one integer");
        let ends = Some((Bound::Int(-40), Bound::Int(90)));
        assert_eq!(table.exact_extremes(0).expect("a column"), ends);
        assert!(table.exact_extremes(1).is_err());
    }

    #[test]
    fn the_nulls_of_a_column_are_added_up_over_the_chunks() {
        let table = counted(VECTOR_SIZE * 2 + 10);
        assert!(table.chunk_count() > 1, "one chunk would not test the fold");
        let rows = table.len();
        let wanted = (0..rows).filter(|row| row % 5 == 0).count();
        assert_eq!(table.null_count(0).expect("the only column"), wanted);
        // A string column that never had a null still answers, with zero.
        let mut words = MemoryTable::new(vec![LogicalType::Varchar]);
        words.append_rows(&[vec![Value::Varchar("a".to_string())]]).expect("one string");
        assert_eq!(words.null_count(0).expect("the only column"), 0);
    }

    #[test]
    fn an_empty_table_has_no_nulls_no_ends_and_a_total_of_nothing() {
        let table = MemoryTable::new(vec![LogicalType::Integer]);
        assert_eq!(table.null_count(0).expect("the only column"), 0);
        assert_eq!(table.exact_extremes(0).expect("the only column"), None);
        // Zero over no rows rather than no answer, which is what a `SUM` of nothing finishes to
        // `NULL` from, because the count beside it is what says there were no rows.
        assert_eq!(table.exact_sum(0).expect("the only column"), Some((0, 0)));
    }

    #[test]
    fn the_ends_and_the_total_are_the_chunks_put_together() {
        let table = counted(VECTOR_SIZE * 2 + 10);
        let rows = table.len();
        let kept: Vec<i128> =
            (0..rows).filter(|row| row % 5 != 0).map(|row| (row % 7) as i128).collect();
        let (total, counted_rows) = table.exact_sum(0).expect("the only column").expect("integers");
        assert_eq!(total, kept.iter().sum::<i128>());
        assert_eq!(counted_rows as usize, kept.len());
        let (low, high) = table.exact_extremes(0).expect("the only column").expect("integers");
        assert_eq!(low, Bound::Int(*kept.iter().min().expect("some rows")));
        assert_eq!(high, Bound::Int(*kept.iter().max().expect("some rows")));
        // The nulls are left out of both, the same way `MIN` and `SUM` leave them out.
        assert_eq!(counted_rows as usize + table.null_count(0).expect("the column"), rows);
    }

    #[test]
    fn a_column_that_cannot_be_added_up_has_no_total_and_still_has_ends() {
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        table
            .append_rows(&[
                vec![Value::Varchar("pear".to_string())],
                vec![Value::Null],
                vec![Value::Varchar("apple".to_string())],
            ])
            .expect("three strings");
        assert_eq!(table.exact_sum(0).expect("the only column"), None);
        let (low, high) = table.exact_extremes(0).expect("the only column").expect("strings");
        assert_eq!(low, Bound::of_value(&Value::Varchar("apple".to_string())).expect("a bound"));
        assert_eq!(high, Bound::of_value(&Value::Varchar("pear".to_string())).expect("a bound"));
    }

    #[test]
    fn a_column_the_table_does_not_have_is_an_error_rather_than_an_empty_answer() {
        let table = people();
        // Two columns, so index two is one past the end. An answer of `None` here would read as the
        // column having no statistics, and the caller would go and read rows that are not there.
        assert!(table.null_count(2).is_err());
        assert!(table.exact_extremes(2).is_err());
        assert!(table.exact_sum(2).is_err());
    }

    #[test]
    fn rows_go_in_and_come_back_out() {
        let table = people();
        assert_eq!(table.len(), 3);
        assert_eq!(table.chunk_count(), 1);
        let chunk = table.read(0, &[0, 1]).expect("both columns of the only chunk");
        assert_eq!(chunk.value_at(0, 1), Value::Varchar("ada".to_string()));
        assert_eq!(chunk.value_at(1, 1), Value::Null);
        assert_eq!(chunk.value_at(2, 0), Value::Integer(3));
    }

    #[test]
    fn a_read_gives_back_only_the_columns_it_was_asked_for() {
        let table = people();
        let chunk = table.read(0, &[1]).expect("the second column");
        assert_eq!(chunk.width(), 1);
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.value_at(2, 0), Value::Varchar("grace".to_string()));
    }

    /// `SELECT count(*) FROM t` reads no columns and still has to be told how many rows there were.
    #[test]
    fn a_read_of_no_columns_still_says_how_many_rows() {
        let table = people();
        let chunk = table.read(0, &[]).expect("no columns");
        assert_eq!(chunk.width(), 0);
        assert_eq!(chunk.len(), 3);
    }

    /// The point of storing a chunk as pages. A read points at the stored values rather than copying
    /// them, asserted on the address, because the values are the same either way.
    #[test]
    fn a_read_points_at_the_stored_values_rather_than_copying_them() {
        use rudb_vector::vector::Data;

        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let rows: Vec<Vec<Value>> = (0..64).map(|n| vec![Value::BigInt(n)]).collect();
        table.append_rows(&rows).expect("bigints");

        let address = |chunk: &Chunk| match chunk.column(0).expect("one column").data() {
            Some(Data::Int64(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("a BIGINT column is not a run of i64"),
        };
        let stored = address(&table.chunk(0).expect("the only chunk"));
        let first = table.read(0, &[0]).expect("the only chunk");
        let second = table.read(0, &[0]).expect("the only chunk again");
        assert_eq!(address(&first), stored, "the read copied the column out");
        assert_eq!(address(&second), stored, "the second read copied the column out");
        assert_eq!(first.value_at(7, 0), Value::BigInt(7));
        assert_eq!(second.value_at(63, 0), Value::BigInt(63));

        // Nothing can write through what it was handed, because a vector has no mutating method at
        // all and the only way at the values is `Buffer::to_mut`, which copies the page out first.
        // So the sharing is safe without a rule anybody has to remember.

        // The memory limit is not told about the page twice. Three holders of one 512 byte page add
        // up to the page rather than to three of it, which is the rule in `Buffer::footprint`.
        let charged = table.chunk(0).expect("the only chunk").footprint()
            + first.footprint()
            + second.footprint();
        assert!(charged < 512 * 2, "{charged} charged for one 512 byte page held three times");
    }

    #[test]
    fn more_rows_than_a_vector_become_more_than_one_chunk() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let rows: Vec<Vec<Value>> =
            (0..VECTOR_SIZE + 5).map(|n| vec![Value::BigInt(n as i64)]).collect();
        table.append_rows(&rows).expect("bigints");
        assert_eq!(table.len(), VECTOR_SIZE + 5);
        assert_eq!(table.chunk_count(), 2);
        let last = table.read(1, &[0]).expect("the second chunk");
        assert_eq!(last.len(), 5);
        assert_eq!(last.value_at(4, 0), Value::BigInt((VECTOR_SIZE + 4) as i64));
    }

    #[test]
    fn a_chunk_of_the_wrong_types_is_caught() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        let wrong = Chunk::new(vec![
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("x".to_string())])
                .expect("a string column"),
        ])
        .expect("one column");
        let error = table.append(wrong).expect_err("a varchar is not an integer");
        assert!(error.message().contains("column 0"), "{error}");
    }

    #[test]
    fn a_row_of_the_wrong_width_is_caught() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Integer]);
        let error =
            table.append_rows(&[vec![Value::Integer(1)]]).expect_err("a row of one is not a row");
        assert!(error.message().contains("row 0"), "{error}");
    }

    /// A table of one bigint column, filled a full chunk at a time.
    fn filled(chunks: usize) -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        for chunk in 0..chunks {
            let held: Vec<Value> = (0..VECTOR_SIZE)
                .map(|row| Value::BigInt((chunk * VECTOR_SIZE + row) as i64))
                .collect();
            let column = Vector::from_values(LogicalType::BigInt, &held).expect("bigints");
            table.append(Chunk::new(vec![column]).expect("one column")).expect("a full chunk");
        }
        table
    }

    /// Every row of every chunk, read back through the chunk numbering a scan uses.
    fn every_row(table: &MemoryTable) -> Vec<Value> {
        (0..table.chunk_count())
            .flat_map(|at| {
                let chunk = table.read(at, &[0]).expect("a chunk the table says it has");
                (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>()
            })
            .collect()
    }

    /// The chunk numbering is what it was, and so are the rows in it. The layout is not.
    #[test]
    fn a_full_group_is_one_page_a_column_and_the_chunks_are_windows_into_it() {
        let chunks = ROWS_PER_GROUP / VECTOR_SIZE;
        let table = filled(chunks + 3);
        assert_eq!(table.chunk_count(), chunks + 3, "the chunk numbering moved");
        assert_eq!(table.group_count(), 1, "the full group did not seal");
        assert_eq!(table.len(), (chunks + 3) * VECTOR_SIZE);
        let wanted: Vec<Value> = (0..table.len()).map(|row| Value::BigInt(row as i64)).collect();
        assert_eq!(every_row(&table), wanted, "the rows moved when the layout did");
    }

    /// What a window is, said on the address, because the values are the same either way.
    #[test]
    fn two_chunks_of_one_group_read_out_of_the_same_run_of_memory() {
        use rudb_vector::vector::Data;

        let table = filled(ROWS_PER_GROUP / VECTOR_SIZE);
        assert_eq!(table.group_count(), 1, "one group to read two chunks out of");
        let address = |chunk: &Chunk| match chunk.column(0).expect("one column").data() {
            Some(Data::Int64(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("a BIGINT column is not a run of i64"),
        };
        let first = address(&table.read(0, &[0]).expect("the first chunk"));
        let second = address(&table.read(1, &[0]).expect("the second chunk"));
        assert_eq!(
            second - first,
            VECTOR_SIZE * size_of::<i64>(),
            "the second chunk is not the first one's neighbour, so the group did not lay"
        );
    }

    /// Rows a group has not claimed yet are still readable, which is what a reader mid insert sees.
    #[test]
    fn the_chunks_of_the_group_still_filling_read_back_before_it_seals() {
        let table = filled(3);
        assert_eq!(table.group_count(), 0, "three chunks is not a group yet");
        assert_eq!(table.chunk_count(), 3);
        let wanted: Vec<Value> = (0..table.len()).map(|row| Value::BigInt(row as i64)).collect();
        assert_eq!(every_row(&table), wanted);
        assert_eq!(table.chunk_len(1), Some(VECTOR_SIZE));
        assert_eq!(table.chunk_len(3), None, "a chunk past the end has no length");
    }

    /// The fallback. An encoded column is worth more than a laid out one, so the run is left alone.
    #[test]
    fn a_column_that_arrives_encoded_keeps_its_form_rather_than_being_flattened_into_a_page() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let chunks = ROWS_PER_GROUP / VECTOR_SIZE;
        for _ in 0..chunks {
            let values = Vector::from_values(
                LogicalType::BigInt,
                &[Value::BigInt(7), Value::BigInt(8), Value::BigInt(9)],
            )
            .expect("three distinct values");
            let codes: Vec<u32> = (0..VECTOR_SIZE).map(|row| (row % 3) as u32).collect();
            let column = Vector::dictionary(codes, values).expect("a dictionary column");
            table.append(Chunk::new(vec![column]).expect("one column")).expect("a full chunk");
        }
        assert_eq!(table.group_count(), chunks, "the dictionaries were laid end to end after all");
        let chunk = table.read(0, &[0]).expect("the first chunk");
        assert_eq!(
            chunk.column(0).expect("one column").form(),
            rudb_vector::Form::Dictionary,
            "the fallback flattened the column it was there to protect"
        );
        let wanted: Vec<Value> =
            (0..VECTOR_SIZE).map(|row| Value::BigInt(7 + (row % 3) as i64)).collect();
        assert_eq!((0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>(), wanted);
    }

    /// The statistics are per chunk and stay per chunk, whatever the rows were laid out as.
    #[test]
    fn a_group_does_not_coarsen_the_zone_maps_of_the_chunks_in_it() {
        let table = filled(ROWS_PER_GROUP / VECTOR_SIZE + 1);
        assert_eq!(table.chunk_count(), table.zones.len(), "a chunk lost its zone map");
        let first = table.zone(0).expect("the first chunk's zone");
        let range = first.column(0).expect("its only column");
        assert_eq!(range.low, Some(Bound::Int(0)), "the first chunk's smallest value");
        assert_eq!(
            range.high,
            Some(Bound::Int(VECTOR_SIZE as i128 - 1)),
            "a zone map that speaks for the whole group rather than for the chunk"
        );
        // And the pruning that rests on them still answers, across the seal and outside it.
        let op = rudb_common::bounds::Op::Equal;
        let probes = [Probe { column: 0, op, value: Bound::Int(7) }];
        assert!(!table.skips(0, &probes), "the chunk holding the value was skipped");
        assert!(table.skips(1, &probes), "the chunk that cannot hold the value was read");
    }

    /// The directory the scan divides its work by. Every chunk in exactly one part, parts in order.
    #[test]
    fn the_groups_say_which_chunks_are_theirs_and_the_run_still_filling_says_so_too() {
        let per_group = ROWS_PER_GROUP / VECTOR_SIZE;
        let table = filled(per_group + 3);
        assert_eq!(table.group_parts(), vec![0..per_group, per_group..per_group + 3]);
        // And with nothing open, there is no trailing part to read an empty run out of.
        let whole = filled(per_group);
        assert_eq!(whole.group_parts(), vec![0..per_group]);
        let empty = MemoryTable::new(vec![LogicalType::BigInt]);
        assert!(empty.group_parts().is_empty());
    }

    /// The coarse level. One test instead of a hundred and twenty, and it has to agree with them.
    #[test]
    fn a_group_is_ruled_out_only_when_every_chunk_in_it_would_have_been() {
        let per_group = ROWS_PER_GROUP / VECTOR_SIZE;
        let table = filled(per_group + 3);
        let op = rudb_common::bounds::Op::Equal;
        let probes = |value: i128| [Probe { column: 0, op, value: Bound::Int(value) }];
        // A value in the sealed group, which is rows 0 to ROWS_PER_GROUP - 1.
        assert!(!table.group_skips(0, &probes(7)));
        assert!(table.group_skips(1, &probes(7)), "the open run cannot hold row 7");
        // A value in the run still filling, which is the part after the last group.
        let inside = ROWS_PER_GROUP as i128 + 100;
        assert!(table.group_skips(0, &probes(inside)), "the sealed group cannot hold it");
        assert!(!table.group_skips(1, &probes(inside)));
        // Past every row, so nothing holds it, and past the parts, where nothing is known.
        assert!(table.group_skips(0, &probes(9_000_000)));
        assert!(table.group_skips(1, &probes(9_000_000)));
        assert!(!table.group_skips(2, &probes(9_000_000)), "an index nothing describes is read");
        // The group has to agree with its chunks, which is the whole correctness condition: a
        // group it rules out is a group every chunk of which is ruled out.
        for at in 0..table.chunk_count() {
            for value in [7_i128, inside, 9_000_000] {
                let group = table.group_parts().iter().position(|part| part.contains(&at));
                let group = group.expect("every chunk is in a part");
                if table.group_skips(group, &probes(value)) {
                    assert!(table.skips(at, &probes(value)), "chunk {at} kept, group {group} not");
                }
            }
        }
    }

    /// The fallback leaves a group per chunk, so the coarse level is the chunk's own zone and the
    /// parts are one chunk each. Nothing gets ruled out that the chunk zones would have kept.
    #[test]
    fn a_run_that_would_not_lay_end_to_end_has_a_part_and_a_zone_per_chunk() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let chunks = ROWS_PER_GROUP / VECTOR_SIZE;
        for chunk in 0..chunks {
            let base = (chunk * VECTOR_SIZE) as i64;
            let values = Vector::from_values(
                LogicalType::BigInt,
                &[Value::BigInt(base), Value::BigInt(base + 1)],
            )
            .expect("two distinct values");
            let codes: Vec<u32> = (0..VECTOR_SIZE).map(|row| (row % 2) as u32).collect();
            let column = Vector::dictionary(codes, values).expect("a dictionary column");
            table.append(Chunk::new(vec![column]).expect("one column")).expect("a full chunk");
        }
        assert_eq!(table.group_count(), chunks, "the run laid after all");
        assert_eq!(table.group_parts(), (0..chunks).map(|at| at..at + 1).collect::<Vec<_>>());
        let op = rudb_common::bounds::Op::Equal;
        let probes = [Probe { column: 0, op, value: Bound::Int(VECTOR_SIZE as i128) }];
        assert!(table.group_skips(0, &probes), "chunk 0 holds 0 and 1, and not that");
        assert!(!table.group_skips(1, &probes), "chunk 1 is the one holding it");
    }

    /// An append that fills several groups lays them together, and a group in the middle that
    /// will not lay leaves a group per chunk there without moving the groups after it.
    #[test]
    fn an_append_of_several_groups_lays_each_and_keeps_one_that_will_not_as_its_chunks() {
        let per = ROWS_PER_GROUP / VECTOR_SIZE;
        let total = 3 * per + 2;
        let chunks = (0..total)
            .map(|chunk| {
                let base = (chunk * VECTOR_SIZE) as i64;
                let flat = (0..VECTOR_SIZE).map(|row| Value::BigInt(base + row as i64));
                let flat = Vector::from_values(LogicalType::BigInt, &flat.collect::<Vec<_>>())
                    .expect("bigints");
                let second = if (per..2 * per).contains(&chunk) {
                    let values = Vector::from_values(
                        LogicalType::BigInt,
                        &[Value::BigInt(base), Value::BigInt(base + 1)],
                    )
                    .expect("two distinct values");
                    let codes: Vec<u32> = (0..VECTOR_SIZE).map(|row| (row % 2) as u32).collect();
                    Vector::dictionary(codes, values).expect("a dictionary column")
                } else {
                    flat.clone()
                };
                Chunk::new(vec![flat, second]).expect("two columns")
            })
            .collect::<Vec<_>>();
        let wanted = |row: usize| {
            let chunk = row / VECTOR_SIZE;
            let base = (chunk * VECTOR_SIZE) as i64;
            let second =
                if (per..2 * per).contains(&chunk) { base + (row % 2) as i64 } else { row as i64 };
            vec![Value::BigInt(row as i64), Value::BigInt(second)]
        };
        for workers in [1, 4] {
            let mut table = MemoryTable::new(vec![LogicalType::BigInt, LogicalType::BigInt]);
            table.append_all(chunks.clone(), workers).expect("the append");
            let mut parts = Vec::with_capacity(per + 3);
            parts.push(0..per);
            parts.extend((per..2 * per).map(|at| at..at + 1));
            parts.push(2 * per..3 * per);
            parts.push(3 * per..total);
            assert_eq!(table.group_count(), per + 2, "on {workers} threads");
            assert_eq!(table.group_parts(), parts, "on {workers} threads");
            let mut row = 0;
            for at in 0..table.chunk_count() {
                let chunk = table.read(at, &[0, 1]).expect("a chunk the table says it has");
                for one in 0..chunk.len() {
                    let got = vec![chunk.value_at(one, 0), chunk.value_at(one, 1)];
                    assert_eq!(got, wanted(row), "row {row} on {workers} threads");
                    row += 1;
                }
            }
            assert_eq!(row, total * VECTOR_SIZE);
        }
    }

    /// Nulls are the thing a laid out page can silently lose, since they live beside the values.
    #[test]
    fn the_nulls_of_a_column_survive_the_rows_being_laid_end_to_end() {
        let table = counted(ROWS_PER_GROUP + VECTOR_SIZE);
        assert!(table.group_count() > 0, "nothing sealed, so nothing was laid");
        let rows = table.len();
        let wanted = (0..rows).filter(|row| row % 5 == 0).count();
        assert_eq!(table.null_count(0).expect("the only column"), wanted);
        let read: Vec<Value> = (0..table.chunk_count())
            .flat_map(|at| {
                let chunk = table.read(at, &[0]).expect("a chunk");
                (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<Value> = (0..rows)
            .map(|row| if row % 5 == 0 { Value::Null } else { Value::Integer(row as i32 % 7) })
            .collect();
        assert_eq!(read, expected, "the values or the nulls moved when the layout did");
    }

    /// A string column, which is the one whose page is views rather than a flat run.
    #[test]
    fn a_string_column_is_laid_into_one_arena_and_cut_back_out_of_it() {
        let mut table = MemoryTable::new(vec![LogicalType::Varchar]);
        let rows: Vec<Vec<Value>> = (0..ROWS_PER_GROUP + 5)
            .map(|row| vec![Value::Varchar(format!("a string of some length number {row}"))])
            .collect();
        table.append_rows(&rows).expect("strings");
        assert_eq!(table.group_count(), 1, "the strings were not laid end to end");
        let chunk = table.read(0, &[0]).expect("the first chunk");
        assert_eq!(
            chunk.column(0).expect("one column").form(),
            rudb_vector::Form::StringView,
            "a flat varchar page copies every byte of every long string in every cut"
        );
        assert_eq!(chunk.value_at(3, 0), rows[3][0]);
        let last = table.chunk_count() - 1;
        let tail = table.read(last, &[0]).expect("the last chunk");
        assert_eq!(tail.value_at(tail.len() - 1, 0), rows[ROWS_PER_GROUP + 4][0]);
    }

    #[test]
    fn an_empty_chunk_is_not_stored() {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        table.append(Chunk::empty(&[LogicalType::Integer])).expect("an empty chunk is allowed");
        assert_eq!(table.chunk_count(), 0);
        assert!(table.is_empty());
    }

    /// Appending a result all at once on four threads leaves the table and its statistics exactly as
    /// appending it a chunk at a time does, across a group seal, an empty chunk and three forms.
    #[test]
    fn appending_on_threads_keeps_the_statistics_of_appending_in_order() {
        // The fourth column brings five new values a chunk, so four runs of it hold 495 values between
        // the first three and cross the tally's cap of 512 in the fourth, while they are absorbed.
        let types = vec![
            LogicalType::BigInt,
            LogicalType::Varchar,
            LogicalType::Varchar,
            LogicalType::BigInt,
        ];
        let words: Vec<Value> = ["apple", "pear", "quince"]
            .iter()
            .map(|word| Value::Varchar((*word).to_string()))
            .collect();
        let dictionary = Vector::from_values(LogicalType::Varchar, &words).expect("three words");
        let mut chunks = Vec::new();
        for at in 0..130_i64 {
            if at == 70 {
                chunks.push(Chunk::empty(&types));
            }
            let numbers: Vec<Value> =
                (0..1024).map(|row| Value::BigInt(at * 7919 + row % 5000)).collect();
            let codes: Vec<u32> = (0..1024).map(|row| (row + at as u32) % 3).collect();
            let named: Vec<Value> = (0..1024)
                .map(|row| {
                    if row % 17 == 0 {
                        Value::Null
                    } else {
                        Value::Varchar(format!("n{}", (at * 31 + row) % 9000))
                    }
                })
                .collect();
            let crossing: Vec<Value> =
                (0..1024).map(|row| Value::BigInt(at * 5 + i64::from(row % 5))).collect();
            chunks.push(
                Chunk::new(vec![
                    Vector::from_values(LogicalType::BigInt, &numbers).expect("numbers"),
                    Vector::dictionary(codes, dictionary.clone()).expect("words"),
                    Vector::from_values(LogicalType::Varchar, &named).expect("names"),
                    Vector::from_values(LogicalType::BigInt, &crossing).expect("crossing"),
                ])
                .expect("a chunk"),
            );
        }
        let mut one = MemoryTable::new(types.clone());
        for chunk in chunks.clone() {
            one.append(chunk).expect("the table's own types");
        }
        let mut all = MemoryTable::new(types);
        all.append_all(chunks, 4).expect("the table's own types");

        assert_eq!(all.len(), one.len());
        assert_eq!(all.chunk_count(), one.chunk_count());
        assert_eq!(all.group_count(), one.group_count());
        for chunk in 0..one.chunk_count() {
            assert_eq!(all.zone(chunk), one.zone(chunk), "the zone of chunk {chunk}");
            let columns = [0, 1, 2, 3];
            let (left, right) = (all.read(chunk, &columns), one.read(chunk, &columns));
            let (left, right) = (left.expect("a chunk"), right.expect("a chunk"));
            for column in columns {
                for row in 0..left.len() {
                    assert_eq!(
                        left.column(column).expect("a column").value_at(row),
                        right.column(column).expect("a column").value_at(row),
                    );
                }
            }
        }
        for column in 0..4 {
            assert_eq!(
                all.distinct_estimate(column),
                one.distinct_estimate(column),
                "column {column}"
            );
            assert_eq!(all.frequencies(column), one.frequencies(column), "column {column}");
        }
        assert!(one.frequencies(1).expect("frequencies").is_some(), "the words stay under the cap");
        assert!(
            one.frequencies(3).expect("frequencies").is_none(),
            "the fourth column crosses the cap"
        );
        assert!(all.counts_ns() <= all.stats_ns(), "a part is larger than the whole");
    }

    fn trickled(rows: i32) -> MemoryTable {
        let mut table = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Varchar]);
        for id in 0..rows {
            let name =
                if id % 7 == 0 { Value::Null } else { Value::Varchar(format!("n{}", id % 5)) };
            table.append_rows(&[vec![Value::Integer(id), name]]).expect("one row");
        }
        table
    }

    #[test]
    fn rows_that_arrive_one_at_a_time_are_read_as_one_chunk() {
        let table = trickled(100);
        assert_eq!(table.len(), 100);
        assert_eq!(table.chunk_count(), 1);
        assert_eq!(table.chunk_len(0), Some(100));
        assert_eq!(table.group_parts(), vec![0..1]);
        assert_eq!(table.group_rows(0), 100);
        let got = every_row(&table);
        assert_eq!(got, (0..100).map(Value::Integer).collect::<Vec<_>>());
        let names = table.read(0, &[1]).expect("the tail");
        assert_eq!(names.column(0).expect("a column").value_at(0), Value::Null);
        assert_eq!(names.column(0).expect("a column").value_at(3), Value::Varchar("n3".into()));
    }

    #[test]
    fn the_tail_keeps_the_statistics_a_chunk_of_the_same_rows_would_have() {
        let table = trickled(1000);
        let mut whole = MemoryTable::new(vec![LogicalType::Integer, LogicalType::Varchar]);
        let rows: Vec<Vec<Value>> = (0..1000)
            .map(|id| {
                let name =
                    if id % 7 == 0 { Value::Null } else { Value::Varchar(format!("n{}", id % 5)) };
                vec![Value::Integer(id), name]
            })
            .collect();
        whole.append_rows(&rows).expect("rows");
        assert_eq!(table.chunk_count(), whole.chunk_count());
        assert_eq!(table.zone(0), whole.zone(0));
        assert_eq!(table.group_zone(0), whole.group_zone(0));
        for column in 0..2 {
            assert_eq!(table.distinct_values(column), whole.distinct_values(column));
            assert_eq!(table.null_count(column).ok(), whole.null_count(column).ok());
            assert_eq!(table.exact_extremes(column).ok(), whole.exact_extremes(column).ok());
            assert_eq!(table.exact_sum(column).ok(), whole.exact_sum(column).ok());
        }
    }

    #[test]
    fn a_full_tail_becomes_a_chunk_and_a_big_chunk_closes_the_tail_first() {
        let rows = i32::try_from(VECTOR_SIZE).expect("small") + 10;
        let mut table = trickled(rows);
        assert_eq!(table.chunk_count(), 2);
        assert_eq!(table.chunk_len(0), Some(VECTOR_SIZE));
        assert_eq!(table.chunk_len(1), Some(10));
        let big: Vec<Vec<Value>> =
            (0..100).map(|id| vec![Value::Integer(rows + id), Value::Null]).collect();
        table.append_rows(&big).expect("rows");
        assert_eq!(table.chunk_count(), 3);
        assert_eq!(table.chunk_len(1), Some(10));
        assert_eq!(table.chunk_len(2), Some(100));
        let got = every_row(&table);
        assert_eq!(got, (0..rows + 100).map(Value::Integer).collect::<Vec<_>>());
    }

    /// Appends of a row a statement go on the tail, which stays a few chunks, rather than each
    /// becoming a chunk of its own.
    #[test]
    fn small_appends_share_the_tail_and_it_stays_a_few_chunks() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let mut table = MemoryTable::new(types);
        let rows = VECTOR_SIZE + 300;
        for id in 0..rows {
            let id = i64::try_from(id).expect("small");
            let columns = vec![
                Vector::from_values(LogicalType::BigInt, &[Value::BigInt(id)]).expect("ids"),
                Vector::from_values(LogicalType::Varchar, &[Value::Varchar(format!("n{id}"))])
                    .expect("names"),
            ];
            table.append_all(vec![Chunk::with_rows(columns, 1).expect("a chunk")], 4).expect("in");
            assert!(table.tail.len() < TAIL_CHUNKS, "{} chunks in the tail", table.tail.len());
        }
        assert_eq!(table.chunk_count(), 2);
        assert_eq!(table.chunk_len(0), Some(VECTOR_SIZE));
        assert_eq!(table.chunk_len(1), Some(300));
        let mut seen = 0;
        for chunk in 0..table.chunk_count() {
            let read = table.read(chunk, &[0, 1]).expect("read");
            for row in 0..read.len() {
                assert_eq!(read.value_at(row, 0), Value::BigInt(seen));
                assert_eq!(read.value_at(row, 1), Value::Varchar(format!("n{seen}")));
                seen += 1;
            }
        }
        assert_eq!(seen, i64::try_from(rows).expect("small"));
    }

    #[test]
    fn a_row_handed_over_lands_as_a_row_copied_in() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let rows: Vec<Vec<Value>> =
            (0..300).map(|id| vec![Value::BigInt(id), Value::Varchar(format!("n{id}"))]).collect();
        let (mut taken, mut copied) = (MemoryTable::new(types.clone()), MemoryTable::new(types));
        for row in &rows {
            taken.append_row(row).expect("a row");
            copied.append_rows(std::slice::from_ref(row)).expect("a row");
        }
        assert_eq!(taken.chunk_count(), copied.chunk_count());
        for chunk in 0..taken.chunk_count() {
            assert_eq!(taken.zone(chunk), copied.zone(chunk));
            let (left, right) = (taken.read(chunk, &[0, 1]), copied.read(chunk, &[0, 1]));
            let (left, right) = (left.expect("read"), right.expect("read"));
            for row in 0..left.len() {
                assert_eq!(left.value_at(row, 1), right.value_at(row, 1));
            }
        }
        assert_eq!(taken.distinct_values(1), copied.distinct_values(1));
        assert!(taken.append_row(&[Value::BigInt(1)]).is_err(), "one value for two columns");
        assert_eq!(taken.len(), 300);
    }

    #[test]
    fn rows_and_small_chunks_in_one_tail_read_and_count_as_a_bulk_load() {
        let types = vec![
            LogicalType::BigInt,
            LogicalType::Double,
            LogicalType::Varchar,
            LogicalType::Date,
            LogicalType::Decimal { width: 12, scale: 2 },
        ];
        let row = |id: i64| {
            let name =
                if id % 3 == 0 { Value::Null } else { Value::Varchar(format!("s{}", id % 11)) };
            vec![
                Value::BigInt(id - 50),
                Value::Double(id as f64 / 4.0),
                name,
                Value::Date(i32::try_from(id).expect("small") * 3),
                Value::Decimal { unscaled: i128::from(id) * 7, width: 12, scale: 2 },
            ]
        };
        let rows: Vec<Vec<Value>> = (0..700).map(row).collect();
        let mut mixed = MemoryTable::new(types.clone());
        let mut at = 0;
        while at < rows.len() {
            // Runs of single rows broken up by small chunks of up to forty rows.
            let take = if at % 5 == 0 { (at % 40 + 2).min(rows.len() - at) } else { 1 };
            mixed.append_rows(&rows[at..at + take]).expect("rows");
            at += take;
        }
        let mut whole = MemoryTable::new(types.clone());
        whole.append_rows(&rows).expect("rows");
        assert_eq!(mixed.chunk_count(), 1);
        assert_eq!(mixed.zone(0), whole.zone(0));
        let columns: Vec<usize> = (0..types.len()).collect();
        let (left, right) =
            (mixed.read(0, &columns).expect("read"), whole.read(0, &columns).expect("read"));
        for row in 0..rows.len() {
            for column in 0..types.len() {
                assert_eq!(
                    left.value_at(row, column),
                    right.value_at(row, column),
                    "{row} {column}"
                );
            }
        }
        for column in 0..types.len() {
            assert_eq!(mixed.distinct_values(column), whole.distinct_values(column));
            assert_eq!(mixed.null_count(column).ok(), whole.null_count(column).ok());
            assert_eq!(mixed.exact_extremes(column).ok(), whole.exact_extremes(column).ok());
            assert_eq!(mixed.exact_sum(column).ok(), whole.exact_sum(column).ok());
        }
    }

    #[test]
    fn rows_read_where_they_are_are_the_rows_of_the_chunk_read_whole() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let row = |id: usize| {
            let name = if id % 7 == 0 { Value::Null } else { Value::Varchar(format!("name {id}")) };
            vec![Value::BigInt(i64::try_from(id).expect("small")), name]
        };
        let mut table = MemoryTable::new(types);
        // A sealed group, a chunk of the next one, small chunks in the tail and rows being built.
        let big: Vec<Vec<Value>> = (0..ROWS_PER_GROUP + VECTOR_SIZE).map(row).collect();
        table.append_rows(&big).expect("rows");
        let mut id = big.len();
        for take in [20, 1, 30, 3, 1] {
            let rows: Vec<Vec<Value>> = (id..id + take).map(row).collect();
            if take > TAIL_BUILDS {
                let columns = (0..2)
                    .map(|column| {
                        let values: Vec<Value> =
                            rows.iter().map(|row| row[column].clone()).collect();
                        Vector::from_values(table.types[column].clone(), &values).expect("column")
                    })
                    .collect();
                let chunk = Chunk::with_rows(columns, take).expect("a chunk");
                table.append_all(vec![chunk], 1).expect("a chunk");
            } else {
                table.append_rows(&rows).expect("rows");
            }
            id += take;
        }
        assert!(table.group_count() == 1 && !table.tail.is_empty() && table.built > 0);
        for chunk in 0..table.chunk_count() {
            let len = table.chunk_len(chunk).expect("a chunk");
            let whole = table.read(chunk, &[1, 0]).expect("read");
            let picks: Vec<Vec<u32>> = vec![
                vec![0],
                vec![u32::try_from(len - 1).expect("small")],
                (0..u32::try_from(len).expect("small")).step_by(3).collect(),
                (19..u32::try_from(len.min(24)).expect("small")).collect(),
                (51..u32::try_from(len.min(54)).expect("small")).collect(),
                Vec::new(),
            ];
            for positions in picks {
                let read = table.read_rows(chunk, &[1, 0], &positions).expect("read rows");
                assert_eq!(read.len(), positions.len());
                for (at, &position) in positions.iter().enumerate() {
                    for column in 0..2 {
                        assert_eq!(
                            read.value_at(at, column),
                            whole.value_at(position as usize, column),
                            "chunk {chunk} row {position} column {column}"
                        );
                    }
                }
            }
            assert!(table.read_rows(chunk, &[0], &[u32::try_from(len).expect("small")]).is_err());
        }
    }

    #[test]
    fn a_row_of_the_tail_is_written_where_it_is() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let mut table = MemoryTable::new(types.clone());
        let mut model: Vec<Vec<Value>> = Vec::new();
        let row = |id: i64| vec![Value::BigInt(id), Value::Varchar(format!("n{id}"))];
        let mut next = 0_i64;
        // Rows one at a time and in small chunks, past a vector so the tail closes once, with a
        // write after every few of them to a row of the tail, being built or already a chunk.
        while model.len() < VECTOR_SIZE + 400 {
            if next % 7 == 0 {
                let rows: Vec<Vec<Value>> = (next..next + 5).map(row).collect();
                table.append_rows(&rows).expect("rows");
                model.extend(rows);
                next += 5;
            } else {
                table.append_row(&row(next)).expect("a row");
                model.push(row(next));
                next += 1;
            }
            if next % 3 == 0 {
                let last = table.chunk_count() - 1;
                let rows = table.chunk_len(last).expect("a chunk");
                let start = model.len() - rows;
                let place = usize::try_from(next).expect("small") * 13 % rows;
                let name =
                    if next % 2 == 0 { Value::Null } else { Value::Varchar(format!("w{next}")) };
                let id = Value::BigInt(-next);
                let put = table.put_row(last, place, &[1, 0], &[name.clone(), id.clone()]);
                assert!(put.expect("written"), "row {place} of {rows} in the tail");
                model[start + place] = vec![id, name];
            }
        }
        assert_eq!(table.len(), model.len());
        assert_eq!(table.chunk_count(), 2);
        let mut seen = Vec::new();
        for chunk in 0..table.chunk_count() {
            let read = table.read(chunk, &[0, 1]).expect("read");
            seen.extend(
                (0..read.len()).map(|row| vec![read.value_at(row, 0), read.value_at(row, 1)]),
            );
        }
        assert_eq!(seen, model);
        let mut whole = MemoryTable::new(types);
        whole.append_rows(&model).expect("rows");
        for column in 0..2 {
            assert_eq!(table.null_count(column).ok(), whole.null_count(column).ok(), "{column}");
        }
        // And the first chunk, which is a chunk of its own now, is written too.
        assert!(table.put_row(0, 5, &[0], &[Value::BigInt(7)]).expect("written"));
        assert_eq!(table.read(0, &[0]).expect("read").value_at(5, 0), Value::BigInt(7));
    }

    #[test]
    fn a_group_filled_a_row_at_a_time_seals_into_one_run() {
        let mut table = MemoryTable::new(vec![LogicalType::BigInt]);
        let rows = ROWS_PER_GROUP + 5;
        for id in 0..rows {
            table.append_rows(&[vec![Value::BigInt(id as i64)]]).expect("one row");
        }
        assert_eq!(table.group_count(), 1);
        assert_eq!(table.chunk_count(), ROWS_PER_GROUP / VECTOR_SIZE + 1);
        assert_eq!(table.group_rows(1), 5);
        assert_eq!(
            table.exact_sum(0).ok().flatten().map(|(sum, _)| sum),
            Some((0..rows as i128).sum())
        );
    }

    #[test]
    fn the_coded_rows_are_counted_again_after_more_rows_come() {
        let types = vec![LogicalType::BigInt, LogicalType::Varchar];
        let words: Vec<Value> =
            ["apple", "pear"].iter().map(|word| Value::Varchar((*word).to_string())).collect();
        let dictionary = Vector::from_values(LogicalType::Varchar, &words).expect("two words");
        let chunk = || {
            let numbers: Vec<Value> = (0..VECTOR_SIZE as i64).map(Value::BigInt).collect();
            let codes: Vec<u32> = (0..VECTOR_SIZE as u32).map(|row| row % 2).collect();
            Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &numbers).expect("numbers"),
                Vector::dictionary(codes, dictionary.clone()).expect("words"),
            ])
            .expect("a chunk")
        };
        let mut table = MemoryTable::new(types);
        let fill = |table: &mut MemoryTable| {
            for _ in 0..ROWS_PER_GROUP / VECTOR_SIZE {
                table.append(chunk()).expect("the table's own types");
            }
        };
        fill(&mut table);
        let (first, rows) = table.dictionary_rows(1).expect("the words");
        assert!(first > 0 && first <= rows, "{first} of {rows}");
        assert_eq!(table.dictionary_rows(0).expect("the numbers").0, 0);
        fill(&mut table);
        let (again, more) = table.dictionary_rows(1).expect("the words");
        assert!(again > first && more > rows, "{again} of {more} after {first} of {rows}");
        assert!(table.dictionary_rows(2).is_err());
    }
}
