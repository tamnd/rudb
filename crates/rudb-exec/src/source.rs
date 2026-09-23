//! The operators that produce rows without an input: the scan, the dummy and the literal rows.
//!
//! These are the bottom of every pipeline and they are all [`Source`] implementations, which is a
//! different shape from the operators above them. A source is shared rather than owned: one object
//! answers [`Source::morsel`] for every thread running the pipeline, so the position it is up to
//! lives in an atomic rather than in a field somebody mutates. What a morsel covers is the source's
//! own business, and the four here mean four different things by it, which is why the type carries
//! numbers and not rows.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_catalog::Table;
use rudb_common::{Error, Field, LogicalType, Result, Session, Value};
use rudb_csv::{Part, Reader as CsvReader, Split};
use rudb_functions::{
    FILE_ROW_NUMBER, Given, TableFunction, csv_given, open_csv, open_parquet, series_length,
};
use rudb_graph::Rids;
use rudb_kernels::{Stepping, cast, moment_steps};
use rudb_metrics::Counters;
use rudb_parquet::{Bound, Op, Reader, Test, skips};
use rudb_pipeline::{Compaction, Gauge, Morsel, Progress, Source, narrow};
use rudb_plan::{ConjunctionOp, Expr, ExprRef, Plan, Slice};
use rudb_seam::{Context, SeamId, Settings};
use rudb_storage::Probe;
use rudb_vector::{Chunk, Data, Selection, VECTOR_SIZE, Vector};

use crate::cutoff::Cutoff;
use crate::expr::{evaluate_all, evaluate_all_in_time_zone};
use crate::prepared::{Prepared, Scratch};
use crate::register::compaction;
use crate::schema::Schema;
use crate::sideways::Sideways;
use crate::stream::later_passes;
use crate::table::{Across, hash};

/// One morsel per position, handed to whoever asks first.
///
/// The three sources that already have their chunks, or can read one by number, hand out a morsel
/// per chunk, and this is the whole of the sharing that needs: a counter, one fetch and add per
/// morsel, and no lock held while anything is read. It is what makes the difference between two
/// threads scanning a table and two threads waiting for each other.
#[derive(Debug)]
pub(crate) struct Handout {
    next: AtomicU64,
    total: u64,
}

impl Handout {
    /// A handout over `total` positions.
    pub(crate) fn new(total: usize) -> Self {
        Self { next: AtomicU64::new(0), total: u64::try_from(total).unwrap_or(u64::MAX) }
    }

    /// How many positions there are in all, which is how many morsels this will ever hand out.
    pub(crate) fn total(&self) -> usize {
        usize::try_from(self.total).unwrap_or(usize::MAX)
    }

    /// The next position, as a morsel covering it, or `None` when they are all taken.
    pub(crate) fn take(&self) -> Option<Morsel> {
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        (at < self.total).then(|| Morsel::new(at, at, at + 1))
    }

    /// The next position, as a number rather than as a morsel covering it.
    ///
    /// For a source whose morsel covers something other than the position it counted, which is a
    /// scan handing out stripes: it counts stripes here and looks up the parts they hold.
    pub(crate) fn number(&self) -> Option<u64> {
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        (at < self.total).then_some(at)
    }
}

/// Where a morsel has got to, as an index into whatever the source counts.
pub(crate) fn position(morsel: &Morsel) -> usize {
    usize::try_from(morsel.cursor()).unwrap_or(usize::MAX)
}

/// A grouped count answered from a native file's certified frequency synopsis.
#[derive(Debug)]
pub(crate) struct Frequencies {
    chunks: Vec<Chunk>,
    handout: Handout,
}

impl Frequencies {
    /// Builds exact aggregate rows supplied by a certified native derived-group synopsis.
    pub(crate) fn records(schema: Schema, records: Vec<Vec<Value>>) -> Result<Self> {
        let types = schema.types();
        let mut chunks = Vec::with_capacity(records.len().div_ceil(VECTOR_SIZE));
        for records in records.chunks(VECTOR_SIZE) {
            let mut columns = vec![Vec::with_capacity(records.len()); types.len()];
            for record in records {
                if record.len() != types.len() {
                    return Err(Error::internal("a stored aggregate row has the wrong width"));
                }
                for (values, value) in columns.iter_mut().zip(record) {
                    values.push(value.clone());
                }
            }
            let vectors = columns
                .into_iter()
                .zip(&types)
                .map(|(values, ty)| Vector::from_values(ty.clone(), &values))
                .collect::<Result<Vec<_>>>()?;
            chunks.push(Chunk::with_rows(vectors, records.len())?);
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { chunks, handout })
    }

    /// Builds already evaluated multi-column grouped counts.
    pub(crate) fn grouped(schema: Schema, entries: Vec<(Vec<Value>, u64)>) -> Result<Self> {
        let output_types = schema.types();
        let Some((&LogicalType::BigInt, group_types)) = output_types.split_last() else {
            return Err(Error::internal("a grouped frequency source count is not BIGINT"));
        };
        let mut chunks = Vec::with_capacity(entries.len().div_ceil(VECTOR_SIZE));
        for entries in entries.chunks(VECTOR_SIZE) {
            let mut columns = vec![Vec::with_capacity(entries.len()); group_types.len()];
            let mut counts = Vec::with_capacity(entries.len());
            for (keys, count) in entries {
                if keys.len() != group_types.len() {
                    return Err(Error::internal("a grouped frequency key has the wrong width"));
                }
                for (column, key) in columns.iter_mut().zip(keys) {
                    column.push(key.clone());
                }
                counts.push(
                    i64::try_from(*count)
                        .map_err(|_| Error::internal("a stored frequency exceeds BIGINT"))?,
                );
            }
            let mut output = columns
                .into_iter()
                .zip(group_types)
                .map(|(values, ty)| Vector::from_values(ty.clone(), &values))
                .collect::<Result<Vec<_>>>()?;
            output.push(Vector::flat(LogicalType::BigInt, Data::Int64(counts.into()))?);
            chunks.push(Chunk::with_rows(output, entries.len())?);
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { chunks, handout })
    }

    /// Builds the aggregate rows which the ordinary TopN above this source will order and limit.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        schema: Schema,
        groups: Slice,
        column: usize,
        entries: Vec<(Value, u64)>,
        session: &Session,
    ) -> Result<Self> {
        let output_types = schema.types();
        let group_exprs = plan.expr_list(groups);
        if output_types.len() != group_exprs.len() + 1
            || output_types.last() != Some(&LogicalType::BigInt)
        {
            return Err(Error::internal("a frequency source count is not BIGINT"));
        }
        let input_types = input.types();
        let key_type = input_types
            .get(column)
            .ok_or_else(|| Error::internal("a frequency column is outside its scan"))?;
        let mut chunks = Vec::with_capacity(entries.len().div_ceil(VECTOR_SIZE));
        for entries in entries.chunks(VECTOR_SIZE) {
            let mut keys = Vec::with_capacity(entries.len());
            let mut counts = Vec::with_capacity(entries.len());
            for (key, count) in entries {
                keys.push(key.clone());
                counts.push(
                    i64::try_from(*count)
                        .map_err(|_| Error::internal("a stored frequency exceeds BIGINT"))?,
                );
            }
            let mut columns = input_types
                .iter()
                .map(|ty| Vector::constant(ty.clone(), Value::Null, keys.len()))
                .collect::<Vec<_>>();
            columns[column] = Vector::from_values(key_type.clone(), &keys)?;
            let input_chunk = Chunk::with_rows(columns, keys.len())?;
            let mut output = evaluate_all_in_time_zone(
                plan,
                group_exprs,
                input,
                &input_chunk,
                session.session_time_zone(),
            )?;
            output.push(Vector::flat(LogicalType::BigInt, Data::Int64(counts.into()))?);
            chunks.push(Chunk::with_rows(output, keys.len())?);
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { chunks, handout })
    }
}

impl Source for Frequencies {
    fn morsel(&self) -> Option<Morsel> {
        self.handout.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.handout.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let at = position(morsel);
        *out = self
            .chunks
            .get(at)
            .ok_or_else(|| Error::internal("a frequency morsel is out of range"))?
            .clone();
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

/// A whole-table aggregate answered from what the file already wrote down.
///
/// One row, worked out before the pipeline starts, so there is nothing to scan and nothing to
/// combine. What can be answered this way is decided in the builder, and this only carries the
/// answer, because the interesting part is which questions a file can settle and not how a single
/// row is handed out.
#[derive(Debug)]
pub(crate) struct Summary {
    row: Chunk,
    handout: Handout,
}

impl Summary {
    /// The one row, with a value per aggregate in the order the aggregates were written.
    pub(crate) fn new(schema: &Schema, values: &[Value]) -> Result<Self> {
        let types = schema.types();
        if types.len() != values.len() {
            return Err(Error::internal("a summary has a different number of values and columns"));
        }
        let columns = types
            .iter()
            .zip(values)
            .map(|(ty, value)| Vector::from_values(ty.clone(), std::slice::from_ref(value)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { row: Chunk::with_rows(columns, 1)?, handout: Handout::new(1) })
    }
}

impl Source for Summary {
    fn morsel(&self) -> Option<Morsel> {
        self.handout.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.handout.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = self.row.clone();
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while reading a file")
}

/// A base table scan.
///
/// `columns` is the position in the stored table of each column the plan asked for, worked out once
/// when the operator is built. The plan's projection is a list of fields and the table's columns are
/// a list of fields, and they are the same list today only because the binder projects every column
/// in order. Resolving by name rather than assuming that is what keeps this operator correct after
/// projection pushdown makes the plan's list a subset, which is the M1 change section 9.2 describes
/// as the difference between 20 GB and 200 MB on ClickBench.
///
/// A morsel here is one stored chunk, because that is the unit the table hands back and reading
/// half of one costs the same as reading all of it. Over a native file with enough stripes to go
/// round it is a whole stripe instead, which is sixty four chunks, for the reason [`Scan::stripes`]
/// gives.
///
/// `probes` is what the filter above this scan already knows, in the same shape [`FileScan`] takes
/// it, and it is answered against the table's zone maps a chunk at a time. That is a finer unit than
/// the Parquet path gets: a row group on the files this engine is measured against is a hundred
/// thousand rows and a chunk is two thousand and forty eight, and on a selective filter over a
/// clustered column that is most of the difference between the two paths.
///
/// `sideways` is the other source of the same kind of test, and it is one a plan cannot carry: the
/// range of the key a join above this scan is about to look every one of these rows up by, which is
/// not known until that join's other side has finished. See [`crate::sideways`].
///
/// `cutoff` is a third, and it is the only one that keeps changing while the scan runs: how good a
/// row has to be to still be wanted by the top N above. See [`crate::cutoff`].
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    table: &'a Table,
    columns: Vec<Option<usize>>,
    offsets: Vec<i64>,
    probes: Vec<Probe>,
    /// The runtime filter of the join this scan drives, empty for a scan that drives no join.
    sideways: Option<Arc<Sideways<'a>>>,
    /// The runtime filters of joins further up that reached this scan through the joins between,
    /// each with its own count of whether it is paying. Only their ranges, bitmaps and filters are
    /// read. The exact rows a join can hand down are positions counted from this join's own side,
    /// and those stay with `sideways`.
    also: Vec<(Arc<Sideways<'a>>, Paying)>,
    /// The cutoff of the top N above this scan, empty for a scan with no top N that could use one.
    cutoff: Option<Arc<Cutoff>>,
    /// Which table index this scan's columns bind against, which is how the runtime filter knows
    /// whether it is about one of them.
    index: u32,
    /// `probes` and whatever the runtime filter turned out to hold, worked out the first time the
    /// scan is asked anything.
    ///
    /// Once rather than per chunk, and lazily rather than at construction, because the one moment
    /// both are known is after the pipeline this one depends on has finished and before this one has
    /// started. That is [`Source::morsels`], and every read is after it.
    testing: OnceLock<Vec<Probe>>,
    schema: Schema,
    chunks: Handout,
    /// The parts of each stripe, empty when the rows are not native.
    ///
    /// A part is a quarter of a megabyte of a column and a stripe holds sixty four of them under
    /// one page, so a worker that takes a part takes a sixty fourth of a read. Handing parts out
    /// one at a time puts every worker in the same stripe at the same moment, and the reader can
    /// only let one of them read the page: the other fifteen read their own part out of the same
    /// bytes, and the winner reads those bytes again as part of the page. On the ClickBench file a
    /// cold column comes off the disk at 550 MB/s that way, on a disk that does 4 GB/s with one
    /// reader and 7 GB/s with eight, because a quarter of the bytes move twice and the rest move in
    /// reads too small to keep the queue full.
    ///
    /// A morsel that is a whole stripe fixes it at the source. Each worker owns the page it reads,
    /// nobody loses a race, and sixteen workers put sixteen quarter megabyte reads in flight.
    stripes: Vec<Range<usize>>,
    /// The filter this scan applies to the rows it read, when the builder gave it one.
    pushed: Option<Pushed>,
    /// How many chunks the zone maps proved the filter keeps whole, so the comparison never ran.
    waved: AtomicUsize,
    /// The runs of parts a morsel covers, used instead of `chunks` once it is there.
    ///
    /// Empty until [`Source::morsels`] fills it, which is the one moment the scan knows how many
    /// workers it will face and so the one moment it can decide how finely to cut.
    spread: OnceLock<Spread>,
    skipped: AtomicUsize,
    /// Whether the runtime filter has been earning the hash it costs.
    /// The projected string columns the pushed filter does not read, by their place in the
    /// projection, which are read after the filters have run and only for the rows they kept. See
    /// [`Self::read_deferring`].
    deferred: Vec<usize>,
    /// What the filters kept of the parts read that way, which stops the deferring once they are
    /// measured keeping most rows, since then the string columns are read nearly whole anyway and
    /// the second read is a cost with nothing to show for it.
    deferring: Paying,
    paying: Paying,
    /// What the pushed filter keeps, which decides whether a Bloom filter runs ahead of it.
    passed: Paying,
    /// The operator row this scan reports its part counts to, when it is being measured.
    ///
    /// The scan has counted its own skips since the walk was written and nobody outside could see
    /// them. They are the number that says whether the physical order of the table is doing any
    /// work, so they go on the operator row.
    counters: Option<Arc<Counters>>,
}

/// How many rows go through the runtime filter before it has to justify itself.
const WARMUP: usize = 1 << 16;

/// Whether the runtime filter is worth the hash it costs, counted as the scan goes.
///
/// A filter that turns most rows away pays for itself many times over: the row it drops is a row the
/// join above does not look up, and a lookup into a build side too large for the cache is several
/// times what the filter charges. A filter that turns none away is a hash and a cache line per row
/// spent on nothing. Which of the two a join has is not something either side knows before the rows
/// go past, so the scan applies the filter, counts what it kept, and stops applying it once enough
/// rows have gone by to say it is not paying.
///
/// Giving up is final. Once the scan stops counting the numbers stop moving, so a filter that failed
/// its warmup is never asked again, and the rest of the table goes past at the speed it would have
/// had if the join had never armed anything.
#[derive(Debug, Default)]
struct Paying {
    seen: AtomicUsize,
    kept: AtomicUsize,
}

impl Paying {
    /// Whether the filter has earned another chunk.
    fn worth(&self) -> bool {
        let seen = self.seen.load(Ordering::Relaxed);
        // Under the warmup there is nothing to go on, and a filter is given the benefit of it.
        seen < WARMUP
            || self.kept.load(Ordering::Relaxed).saturating_mul(4) < seen.saturating_mul(3)
    }

    /// Whether the filter has been measured keeping more than half of what it saw.
    fn loose(&self) -> bool {
        let seen = self.seen.load(Ordering::Relaxed);
        seen >= WARMUP && self.kept.load(Ordering::Relaxed).saturating_mul(2) > seen
    }

    /// Records what one chunk put through the filter and what came out.
    fn saw(&self, rows: usize, kept: usize) {
        self.seen.fetch_add(rows, Ordering::Relaxed);
        self.kept.fetch_add(kept, Ordering::Relaxed);
    }
}

/// Moves tests from the projection's numbering onto the table's.
///
/// A test names a column of the projection and a zone names a column of the table, so the move
/// happens once here rather than at every chunk. A test on a column that is somehow not projected is
/// dropped, which costs a chunk that gets read.
fn onto(columns: &[Option<usize>], tests: Vec<(usize, Op, Bound)>) -> Vec<Probe> {
    tests
        .into_iter()
        .filter_map(|(at, op, value)| {
            Some(Probe { column: columns.get(at)?.as_ref().copied()?, op, value })
        })
        .collect()
}

/// A filter a builder is handing to a scan to apply, rather than one it means to run above it.
///
/// This is what DuckDB calls the table filters of a scan, and the reason to want it is the middle of
/// the three answers a zone map can give. Pruning uses one end of it: the chunk holds nothing the
/// filter wants, so it is never read. The other end is the chunk that holds nothing the filter would
/// throw away, and only an operator that has both the zone and the predicate in front of it can act
/// on that. A filter above the scan has the predicate and not the zone, so it compares every row of
/// every chunk whatever the statistics already proved.
///
/// It is offered rather than pushed, because only a filter sitting directly on a stored table can
/// go. See [`rudb_opt::bounds::into_scan`]. Whether the predicate reads as tests is a separate
/// question and decides only whether the middle answer above is available: a predicate with an `OR`
/// in it still moves down, it just gets compared on every chunk the pruning left alive, which is
/// what it would have done above the scan anyway.
#[derive(Debug)]
pub(crate) struct Pushdown {
    /// The filter node this came from, which is still in the plan and is what the compaction gain
    /// function counts the passes above.
    pub(crate) node: rudb_plan::NodeRef,
    pub(crate) predicate: ExprRef,
    /// The conjuncts, read as tests, in the numbering of what the scan produces.
    pub(crate) tests: Vec<(usize, Op, Bound)>,
    /// Whether those tests are the whole predicate, which is what makes the zone shortcut sound.
    pub(crate) whole: bool,
}

/// Everything the builder hands a scan that narrows what it reads.
///
/// Four things rather than one because they arrive from four places and are used at four moments.
/// The pruning tests come from the predicate and are asked of a zone before a chunk is read. The
/// pushed filter is that same predicate again, kept whole, and is applied to the chunks that were
/// read. The sideways filter comes from a join that has already built its side and is asked while the
/// scan is running. The cutoff comes from the top N above and is asked again for every part, because
/// it tightens as the query runs. They are gathered into one argument because a scan takes all four
/// and a constructor of ten parameters is a constructor nobody reads.
#[derive(Debug, Default)]
pub(crate) struct Filters<'a> {
    /// Tests a zone map can rule a chunk out with, which is as many of the conjuncts as could be
    /// read. One that is missing costs a chunk that gets read and never costs a row.
    pub(crate) pruning: Vec<(usize, Op, Bound)>,
    /// The whole predicate, for the scan to apply, or `None` when a filter above it is applying it.
    pub(crate) pushed: Option<Pushdown>,
    /// What a join built and handed back down after the plan was already running.
    pub(crate) sideways: Option<Arc<Sideways<'a>>>,
    /// The same from joins further up, which reached this scan through the joins in between.
    pub(crate) also: Vec<Arc<Sideways<'a>>>,
    /// How good a row has to be to still interest the top N above, which moves while the scan runs.
    pub(crate) cutoff: Option<Arc<Cutoff>>,
}

/// How many readers can hold working space for a pushed filter without ever waiting on each other.
///
/// Sixty four, the way the radix partitions are sixty four. It is more threads than any machine the
/// engine runs a single pipeline on, and few enough slots that a scan can make them all up front.
/// A machine with more threads than this wraps and two of its readers share a slot, which is still
/// correct and is slow in exactly the way one shared free list was.
const SLOTS: usize = 64;

/// The next reader number to hand out, counted across the process rather than per query.
static READERS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's reader number, assigned the first time it asks for one and kept for its life.
    ///
    /// A thread identifier that turned into a small index would do instead and `ThreadId` is not
    /// one. Pool threads are made once and run every query, so this is assigned once per thread for
    /// the life of the process and read out of a `Cell` after that.
    static READER: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Drops the rows of a chunk that the exact rows from a join above do not hold, for a scan with no
/// filter to fold them into.
///
/// `reduced` is the set and the table position of the chunk's first row. The rows are the part's
/// own at this point, so the row numbers a scan makes up are still in step with them.
///
/// # Errors
///
/// Whatever narrowing the chunk to the rows that survived raises.
fn reduce(reduced: Option<(&Rids, u64)>, chunk: &mut Chunk) -> Result<()> {
    let Some((rows, first)) = reduced else { return Ok(()) };
    let len = chunk.len();
    let kept = Selection::from_predicate(len, |row| rows.contains(first + row as u64));
    if kept.len() == len {
        return Ok(());
    }
    let whole = std::mem::replace(chunk, Chunk::empty(&[]));
    *chunk = whole.select(&kept)?;
    Ok(())
}

/// Which slot the calling thread takes its working space out of.
fn reader() -> usize {
    READER.with(|reader| {
        let mut at = reader.get();
        if at == usize::MAX {
            at = READERS.fetch_add(1, Ordering::Relaxed);
            reader.set(at);
        }
        at % SLOTS
    })
}

/// The filter a scan applies itself, once it has been prepared against the scan's own columns.
#[derive(Debug)]
struct Pushed {
    predicate: Prepared,
    /// A necessary single-column LIKE that can run before the other projected column is read.
    late: Option<Late>,
    compaction: &'static dyn Compaction,
    passes: u32,
    /// The same conjuncts as probes, or `None` when they are not the whole predicate or one of them
    /// could not be moved onto the table's numbering.
    ///
    /// `None` rather than a shorter list, because a list with one conjunct missing from it would
    /// prove the rows pass a filter that is not the one being applied, and [`Zone::certain`] over an
    /// empty list is vacuously true. Wrong by omission is the one failure mode worth a type here. It
    /// costs the comparison on every chunk, which is what the engine did before this existed.
    ///
    /// [`Zone::certain`]: rudb_storage::Zone::certain
    probes: Option<Vec<Probe>>,
    /// Working space kept per reader rather than in one free list they all queue for.
    ///
    /// A [`Source`] has no per instance state to keep this in, which a [`Stream`] does, so the scan
    /// keeps the slots itself and each reader goes to the one its thread was given. It was a single
    /// free list behind a single lock to begin with, and that is a lock taken twice per chunk by
    /// every thread in the pipeline, on every chunk the comparison runs on. Uncontended that is tens
    /// of nanoseconds against a comparison over a thousand rows and it does not matter. Contended it
    /// is the query: moving the ClickBench filters into the scan put nineteen of them onto this path
    /// and three came out slower on ten threads and faster on one, which is the shape of a lock and
    /// not the shape of extra work.
    ///
    /// The lock stays because a slot can still be shared, on a machine with more threads than
    /// [`SLOTS`]. The point is not that there is no lock, it is that taking it never waits.
    ///
    /// Building the working space per chunk instead would be simpler than either, and it is two
    /// allocations of a vector per step of the predicate on a call a scan makes a hundred thousand
    /// times.
    ///
    /// [`Stream`]: rudb_pipeline::Stream
    spare: Vec<Mutex<Option<Working>>>,
}

/// One reader's share of what a pushed filter mutates.
#[derive(Debug)]
struct Working {
    scratch: Scratch,
    late_scratch: Option<Scratch>,
    gauge: Gauge,
}

/// The first predicate and column of a two-column selective scan.
#[derive(Debug)]
struct Late {
    input: usize,
    predicate: Prepared,
    seen: AtomicUsize,
    kept: AtomicUsize,
}

/// Stop paying for the first-column probe when it has not removed enough rows.
const LATE_WARMUP: usize = 1 << 14;

impl Late {
    fn worth(&self) -> bool {
        let seen = self.seen.load(Ordering::Relaxed);
        seen < LATE_WARMUP || self.kept.load(Ordering::Relaxed).saturating_mul(4) < seen
    }

    fn saw(&self, rows: usize, kept: usize) {
        self.seen.fetch_add(rows, Ordering::Relaxed);
        self.kept.fetch_add(kept, Ordering::Relaxed);
    }
}

/// A LIKE conjunct followed by a simple comparison on another column.
///
/// Only a necessary conjunct of an AND is allowed here. The full predicate still runs after the
/// sparse read, so this choice cannot accept a row the ordinary scan would reject.
fn late_like(plan: &Plan, schema: &Schema, predicate: ExprRef) -> Option<(usize, ExprRef)> {
    if schema.width() != 2 {
        return None;
    }
    let Expr::Conjunction { op: ConjunctionOp::And, children } = *plan.expr(predicate) else {
        return None;
    };
    let [left, right] = plan.expr_list(children) else { return None };
    for (candidate, other) in [(*left, *right), (*right, *left)] {
        let Expr::Function { name, args } = *plan.expr(candidate) else { continue };
        if plan.string(name) != "~~" {
            continue;
        }
        let [column, constant] = plan.expr_list(args) else { continue };
        let Expr::Column(binding) = *plan.expr(*column) else { continue };
        if !matches!(*plan.expr(*constant), Expr::Constant(_)) {
            continue;
        }
        let Some(input) = schema.position_of(binding) else { continue };
        let Expr::Compare { left, right, .. } = *plan.expr(other) else { continue };
        let compared = match (plan.expr(left), plan.expr(right)) {
            (Expr::Column(binding), Expr::Constant(_))
            | (Expr::Constant(_), Expr::Column(binding)) => *binding,
            _ => continue,
        };
        if schema.position_of(compared) == Some(1 - input) {
            return Some((input, candidate));
        }
    }
    None
}

impl Pushed {
    /// Prepares the offered predicate against what the scan produces.
    ///
    /// The same three things [`crate::stream::Filter`] builds, because this is that operator moved
    /// rather than a second one written: the predicate prepared over the input's types, the
    /// compaction the session asked for, and how many times the rows it keeps get read again.
    ///
    /// # Errors
    ///
    /// If the predicate does not resolve against the scan's schema, or if the session has pinned the
    /// compaction seam to something that cannot run over these columns. Both are what the filter
    /// above the scan would have raised, at the same moment, for the same reasons.
    fn new(
        plan: &Plan,
        schema: &Schema,
        columns: &[Option<usize>],
        pushdown: Pushdown,
        seams: &Settings,
        session: &Session,
    ) -> Result<Self> {
        let types = schema.types();
        let context = Context::new(SeamId::ChunkCompaction, seams).with_types(&types);
        let compaction = compaction().choose(&context)?.strategy();
        let whole = pushdown.whole;
        let wanted = pushdown.tests.len();
        let probes = onto(columns, pushdown.tests);
        let late = if columns.len() == 2 && columns.iter().all(Option::is_some) {
            late_like(plan, schema, pushdown.predicate)
                .map(|(input, expr)| {
                    Ok::<_, Error>(Late {
                        input,
                        predicate: Prepared::one(plan, expr, schema)?.in_session(session),
                        seen: AtomicUsize::new(0),
                        kept: AtomicUsize::new(0),
                    })
                })
                .transpose()?
        } else {
            None
        };
        Ok(Self {
            predicate: Prepared::one(plan, pushdown.predicate, schema)?.in_session(session),
            late,
            compaction,
            passes: later_passes(plan, pushdown.node),
            probes: (whole && probes.len() == wanted).then_some(probes),
            spare: (0..SLOTS).map(|_| Mutex::new(None)).collect(),
        })
    }

    /// One reader's working space, out of its own slot or newly made.
    fn take(&self, slot: usize) -> Working {
        let waiting = self.spare[slot].lock().ok().and_then(|mut spare| spare.take());
        waiting.unwrap_or_else(|| Working {
            scratch: self.predicate.scratch(),
            late_scratch: self.late.as_ref().map(|late| late.predicate.scratch()),
            gauge: Gauge::new(self.passes),
        })
    }

    /// The working space back into the slot it came out of.
    ///
    /// A lock this cannot take is a lock somebody panicked holding, and the answer to that is to drop
    /// the working space rather than to fail the scan: the next reader builds one and the query
    /// finishes. The gain function loses the counts this thread had gathered, which costs a
    /// compaction decision made on less evidence and costs no rows.
    fn give(&self, slot: usize, working: Working) {
        if let Ok(mut spare) = self.spare[slot].lock() {
            *spare = Some(working);
        }
    }
}

/// How a native scan cuts its parts into morsels.
///
/// A run is a range of part numbers and a morsel covers one run. The ranges are built out of the
/// parts the zone maps leave alive, so a run holds work rather than holding whatever happened to sit
/// next to it, and the handout beside them is what hands one run to each worker that asks.
#[derive(Debug)]
struct Spread {
    runs: Vec<Range<usize>>,
    handout: Handout,
}

impl<'a> Scan<'a> {
    /// A scan of `table` producing the plan's projected columns.
    ///
    /// # Errors
    ///
    /// If the plan asks for a column the table does not have, which means the catalog changed under
    /// a plan that was bound against it.
    pub(crate) fn new(
        plan: &Plan,
        table: &'a Table,
        index: u32,
        projection: Slice,
        filters: Filters<'a>,
        seams: &Settings,
        session: &Session,
    ) -> Result<Self> {
        let Filters { pruning, pushed: pushdown, sideways, also, cutoff } = filters;
        let fields = plan.field_list(projection).to_vec();
        let mut columns = Vec::with_capacity(fields.len());
        for field in &fields {
            if field.name == FILE_ROW_NUMBER {
                columns.push(None);
                continue;
            }
            let position = table.column_index(&field.name).ok_or_else(|| {
                Error::catalog(format!(
                    "Table \"{}\" does not have a column named \"{}\"",
                    table.name().table,
                    field.name
                ))
            })?;
            columns.push(Some(position));
        }
        let probes = onto(&columns, pruning);
        let schema = Schema::numbered(fields, index);
        let chunks = Handout::new(table.rows().chunk_count());
        let mut next = 0_i64;
        let mut offsets = Vec::with_capacity(table.rows().chunk_count());
        for at in 0..table.rows().chunk_count() {
            offsets.push(next);
            next =
                next.saturating_add(i64::try_from(table.rows().chunk_len(at)?).unwrap_or(i64::MAX));
        }
        let stripes = table.rows().stripe_parts();
        let mut read = vec![false; columns.len()];
        if let Some(pushdown) = &pushdown {
            crate::join::columns(plan, pushdown.predicate, &mut |binding| {
                if let Some(at) = schema.position_of(binding) {
                    read[at] = true;
                }
            });
        }
        let types = schema.types();
        let deferred = (0..columns.len())
            .filter(|&at| columns[at].is_some() && !read[at] && types[at] == LogicalType::Varchar)
            .collect();
        let pushed = pushdown
            .map(|pushdown| Pushed::new(plan, &schema, &columns, pushdown, seams, session))
            .transpose()?;
        Ok(Self {
            table,
            columns,
            offsets,
            probes,
            sideways,
            also: also.into_iter().map(|sideways| (sideways, Paying::default())).collect(),
            cutoff,
            index,
            testing: OnceLock::new(),
            schema,
            chunks,
            stripes,
            pushed,
            waved: AtomicUsize::new(0),
            spread: OnceLock::new(),
            skipped: AtomicUsize::new(0),
            counters: None,
            deferred,
            deferring: Paying::default(),
            paying: Paying::default(),
            passed: Paying::default(),
        })
    }

    /// Applies the filter the builder handed over, to the chunk numbered `at`.
    ///
    /// The three way decision, and the one this whole arrangement exists for. A chunk whose zone
    /// proves every row passes is handed on as it was read, with no comparison, no selection and no
    /// narrowing. A chunk the zone cannot decide is compared, which is what a filter above the scan
    /// would have done to every chunk of the table. The third answer, that the chunk holds nothing at
    /// all, was settled before this: [`Source::read`] walks past it and never reads it.
    ///
    /// On a predicate that keeps most of a clustered column this is most of the chunks. The bounds
    /// note in `rudb-common` has the case: `l_shipdate <= '1998-09-02'` keeps ninety eight percent of
    /// TPC-H lineitem, so nearly every chunk of it is one where the comparison was going to keep
    /// every row and the only thing it produced was the knowledge that it had.
    ///
    /// The exact rows a join above handed down go into the same narrowing, see [`Source::reduce`].
    ///
    /// # Errors
    ///
    /// Whatever evaluating the predicate or narrowing the chunk reports.
    fn apply(&self, at: usize, chunk: &mut Chunk) -> Result<()> {
        let reduced = self.reduced(at).filter(|(rows, _)| !rows.is_full());
        let Some(pushed) = self.pushed.as_ref() else { return reduce(reduced, chunk) };
        let whole =
            pushed.probes.as_deref().is_some_and(|probes| self.table.rows().certain(at, probes));
        if whole {
            self.waved.fetch_add(1, Ordering::Relaxed);
            return reduce(reduced, chunk);
        }
        // Taken and given back rather than built here. An empty free list means every other reader
        // is holding one, which is a reader that has not had a turn yet rather than an error.
        let slot = reader();
        let mut working = pushed.take(slot);
        let mut kept = pushed.predicate.evaluate_filter(chunk, &mut working.scratch)?;
        self.passed.saw(chunk.len(), kept.len());
        // After the filter and on its answer rather than on the chunk, so that the filter's kernels
        // read the columns flat as they came off the disk and the chunk is narrowed once. Narrowing
        // it for the reduction first left the filter a selected chunk, and the comparison on a
        // selected column was the slow path on Q3 at SF1, seven hundred times a query.
        if let Some((rows, first)) = reduced {
            let held: Vec<u32> = kept
                .iter()
                .filter(|&row| rows.contains(first + row as u64))
                .filter_map(|row| u32::try_from(row).ok())
                .collect();
            kept = Selection::from_indices(held);
        }
        if kept.len() != chunk.len() {
            narrow(pushed.compaction, chunk, &kept, &mut working.gauge)?;
        }
        pushed.give(slot, working);
        Ok(())
    }

    /// Runs the filter this scan took off the operator above it and the joins' runtime filters over
    /// one chunk it has just read, the exact bitmaps first where it can.
    ///
    /// A bitmap costs a subtraction and a bit test a row and is exact, while the pushed filter
    /// evaluates a predicate and then narrows every column, which unpacks the packed ones. In q03 at
    /// SF1 the date filter keeps half of lineitem and the join to orders keeps about one row in a
    /// hundred of those, so narrowing on the date first unpacked fifty times more rows than the join
    /// ever read. A Bloom filter costs a hash a row, so it goes ahead of the pushed filter only once
    /// that filter has been measured keeping more than half the rows. In q04 the date filter keeps
    /// about two thirds of lineitem and hashing first saved a third of the scan, while in q10 it keeps
    /// a quarter and hashing the whole chunk cost more than the narrowing it saved.
    ///
    /// The graph reduction inside [`Self::apply`] names rows by their place in the part, so a part it
    /// narrows keeps the old order, which lets the reduction see the rows where they were read.
    fn narrow_read(&self, at: usize, out: &mut Chunk) -> Result<()> {
        let placed = self.reduced(at).is_some_and(|(rows, _)| !rows.is_full());
        if placed {
            self.apply(at, out)?;
            return self.sift(out);
        }
        self.sift_exact(out)?;
        if out.is_empty() {
            return Ok(());
        }
        if self.passed.loose() {
            self.sift_hashed(out)?;
            if out.is_empty() {
                return Ok(());
            }
            return self.apply(at, out);
        }
        self.apply(at, out)?;
        self.sift_hashed(out)
    }

    /// Reads the first LIKE column before the other projected column when it can reject most rows.
    ///
    /// The full filter runs on the survivors after the sparse read. A dense result falls back to
    /// the ordinary two-column read, and a scan with a sideways filter keeps its original row
    /// positions rather than entering this path.
    fn read_late(&self, at: usize, out: &mut Chunk) -> Result<bool> {
        let Some(pushed) = &self.pushed else { return Ok(false) };
        let Some(late) = &pushed.late else { return Ok(false) };
        if self.sideways.is_some() || !self.also.is_empty() || !late.worth() {
            return Ok(false);
        }
        let Some(primary) = self.columns[late.input] else { return Ok(false) };
        let Some(secondary) = self.columns[1 - late.input] else { return Ok(false) };
        let read = self.table.rows().read(at, &[primary])?;
        let len = read.len();
        let mut columns = self
            .schema
            .types()
            .into_iter()
            .map(|ty| Vector::constant(ty, Value::Null, len))
            .collect::<Vec<_>>();
        columns[late.input] = read.column(0)?.clone();
        let first = Chunk::with_rows(columns, len)?;
        let slot = reader();
        let mut working = pushed.take(slot);
        let selected = late.predicate.evaluate_filter(
            &first,
            working
                .late_scratch
                .as_mut()
                .ok_or_else(|| Error::internal("a late filter has no scratch"))?,
        )?;
        pushed.give(slot, working);
        late.saw(len, selected.len());
        if selected.len().saturating_mul(4) > len {
            return Ok(false);
        }
        if selected.is_empty() {
            *out = Chunk::empty(&self.schema.types());
            return Ok(true);
        }
        let fetched = self.table.rows().read_selected(at, &[secondary], selected.indices())?;
        let first = read.column(0)?.gather(selected.indices())?;
        let second = fetched.column(0)?.clone();
        let columns = if late.input == 0 { vec![first, second] } else { vec![second, first] };
        *out = Chunk::with_rows(columns, selected.len())?;
        self.apply(at, out)?;
        Ok(true)
    }

    /// Reads the string columns nothing filters on only for the rows the filters keep.
    ///
    /// The other columns are read first, with a null in place of each string column the pushed
    /// filter and the joins' filters do not read and a row number on the end, and the filters run
    /// over that as they would over the whole part. The row numbers that come through are the rows
    /// kept, and the string columns are then read at those rows alone, which for a compressed page
    /// decompresses nothing else. In TPC-H q10 the join to orders keeps a quarter of the customer
    /// scan, and decompressing the name, address, phone and comment of the other three quarters
    /// was a fifth of the query.
    ///
    /// Only for a scan that has a filter of some kind, since without one every row is kept, and only
    /// while the filters are measured dropping a quarter of the rows or more. In q01 the date filter
    /// keeps nearly all of lineitem and reading the two flags a second time cost 7 percent. A graph
    /// reduction names rows by where they were read and a late LIKE has its own path, so those do
    /// not come here.
    fn read_deferring(&self, at: usize, out: &mut Chunk) -> Result<bool> {
        if self.deferred.is_empty() || !self.deferring.worth() || self.reduced(at).is_some() {
            return Ok(false);
        }
        let joins = self.sideways.iter().chain(self.also.iter().map(|(sideways, _)| sideways));
        let mut keys = Vec::new();
        for sideways in joins {
            keys.extend(sideways.domain(self.index).map(|(key, _)| key));
            keys.extend(sideways.sifting(self.index).map(|(key, _)| key));
        }
        if self.pushed.is_none() && keys.is_empty() {
            return Ok(false);
        }
        let deferred: Vec<usize> =
            self.deferred.iter().copied().filter(|at| !keys.contains(at)).collect();
        let first: Vec<usize> = (0..self.columns.len())
            .filter(|at| !deferred.contains(at))
            .filter_map(|at| self.columns[at])
            .collect();
        if deferred.is_empty() || first.is_empty() {
            return Ok(false);
        }
        let read = self.table.rows().read(at, &first)?;
        let len = read.len();
        let types = self.schema.types();
        let mut held = Vec::with_capacity(self.columns.len() + 1);
        let mut real = 0;
        for (place, column) in self.columns.iter().enumerate() {
            if deferred.contains(&place) {
                held.push(Vector::constant(types[place].clone(), Value::Null, len));
            } else if column.is_some() {
                held.push(read.column(real)?.clone());
                real += 1;
            } else {
                held.push(Vector::sequence(self.offsets[at], 1, len));
            }
        }
        held.push(Vector::sequence(0, 1, len));
        *out = Chunk::with_rows(held, len)?;
        self.narrow_read(at, out)?;
        let kept = out.len();
        self.deferring.saw(len, kept);
        let mut columns = std::mem::replace(out, Chunk::empty(&[])).into_columns();
        let numbers =
            columns.pop().ok_or_else(|| Error::internal("the row numbers went missing"))?;
        if kept > 0 {
            // A part every row of which was kept is read whole, which is the plain read with nothing
            // to gather afterwards.
            let wanted: Vec<usize> = deferred.iter().filter_map(|&at| self.columns[at]).collect();
            let fetched = if kept == len {
                self.table.rows().read(at, &wanted)?
            } else {
                let mut block = Vec::new();
                if !numbers.signed_block(&mut block) || block.len() < kept {
                    block = (0..kept)
                        .map(|row| numbers.signed_at(row).and_then(|at| i64::try_from(at).ok()))
                        .collect::<Option<Vec<i64>>>()
                        .ok_or_else(|| Error::internal("a row number is not a row"))?;
                }
                let positions = block[..kept]
                    .iter()
                    .map(|&number| u32::try_from(number))
                    .collect::<std::result::Result<Vec<u32>, _>>()
                    .map_err(|_| Error::internal("a row number is not a row"))?;
                self.table.rows().read_rows(at, &wanted, &positions)?
            };
            for (from, &place) in deferred.iter().enumerate() {
                columns[place] = fetched.column(from)?.clone();
            }
        }
        *out = Chunk::with_rows(columns, kept)?;
        Ok(true)
    }

    /// Drops the rows of one chunk that a join above this scan cannot hold a match for.
    ///
    /// The range above rules out whole chunks and this rules out rows inside the ones that are left,
    /// which is the tier that still says something about a column whose values are spread over their
    /// whole domain. One hash of one column and one cache line touched per row, against a filter
    /// small enough to stay in the cache while a fact table goes past it.
    ///
    /// The column is read out of the chunk this scan has just produced rather than out of the table,
    /// so the position is the projection's and the rows are whatever the chunk holds, including the
    /// row numbers a scan makes up. Nothing to do at all for a scan with no join above it, which is
    /// two loads and a branch per chunk, and nothing to do either once [`Paying`] has decided the
    /// filter is not turning enough rows away to be worth hashing for.
    ///
    /// # Errors
    ///
    /// Whatever narrowing the chunk to the rows that survived raises.
    ///
    /// With more than one join above, every bitmap goes before any filter, because a bitmap costs a
    /// bit a row and a filter costs a hash and a cache line, and a row a bitmap has dropped is a row
    /// no filter has to hash.
    fn sift(&self, chunk: &mut Chunk) -> Result<()> {
        self.sift_exact(chunk)?;
        self.sift_hashed(chunk)
    }

    /// The bitmaps half of [`Self::sift`], which is cheap enough to go in front of the pushed
    /// filter. See [`Self::narrow_read`].
    fn sift_exact(&self, chunk: &mut Chunk) -> Result<()> {
        let handoffs = || {
            let own = self.sideways.iter().map(|sideways| (sideways, &self.paying));
            own.chain(self.also.iter().map(|(sideways, paying)| (sideways, paying)))
        };
        // The bitmap a join over a relationship with no link in the file leaves, or one over integer
        // keys that sit close together. It is exact and costs a subtraction and a bit a row, which
        // is cheap but not free, so it answers to the same count as the filter and a bitmap that
        // keeps nearly every row stops being asked. It takes the place of the filter rather than
        // going in front of it, see `Found::domain`.
        for (sideways, paying) in handoffs() {
            let Some((at, domain)) = sideways.domain(self.index) else { continue };
            if !paying.worth() {
                continue;
            }
            let Ok(column) = chunk.column(at) else { continue };
            let rows = chunk.len();
            let kept = domain.keep(column, rows, &mut Vec::new());
            paying.saw(rows, kept.len());
            if kept.len() < rows {
                let whole = std::mem::replace(chunk, Chunk::empty(&[]));
                *chunk = whole.select(&Selection::from_indices(kept))?;
            }
        }
        Ok(())
    }

    /// The filters half of [`Self::sift`], a hash a row, run after the bitmaps so that a row a
    /// bitmap has dropped is a row no filter has to hash.
    fn sift_hashed(&self, chunk: &mut Chunk) -> Result<()> {
        let handoffs = || {
            let own = self.sideways.iter().map(|sideways| (sideways, &self.paying));
            own.chain(self.also.iter().map(|(sideways, paying)| (sideways, paying)))
        };
        for (sideways, paying) in handoffs() {
            if chunk.is_empty() {
                return Ok(());
            }
            if sideways.domain(self.index).is_some() {
                continue;
            }
            let Some((at, filter)) = sideways.sifting(self.index) else { continue };
            if !paying.worth() {
                continue;
            }
            let Ok(column) = chunk.column(at) else { continue };
            let rows = chunk.len();
            let mut hashes = Vec::new();
            hash(std::slice::from_ref(column), rows, &mut hashes, Across::TwoInputs);
            // The whole chunk asked at once rather than a row at a time inside the selection,
            // because the filter is larger than the cache and a row of it is a trip to memory the
            // core can only overlap with the next row's if nothing in between branches on the
            // answer.
            let mut held = Vec::new();
            filter.holds_run(&hashes, &mut held);
            let kept = Selection::from_predicate(rows, |row| held[row]);
            paying.saw(rows, kept.len());
            if kept.len() < rows {
                let whole = std::mem::replace(chunk, Chunk::empty(&[]));
                *chunk = whole.select(&kept)?;
            }
        }
        Ok(())
    }

    /// Every test this scan has, which is the plan's and whatever a join above it worked out.
    ///
    /// Settled on the first call and the same afterwards, because a filter that answered one thing
    /// while the morsels were cut and another while they were read would be a scan whose work was
    /// divided by one set of rows and done over a different one.
    fn testing(&self) -> &[Probe] {
        self.testing.get_or_init(|| {
            let mut probes = self.probes.clone();
            if let Some(sideways) = self.sideways.as_ref() {
                // Here because this is the one moment every instance of the scan passes through
                // after the build side has finished and before a row is read.
                if let (Some(counters), Some(reduced)) =
                    (&self.counters, sideways.reduction(self.index))
                {
                    counters.reducing(reduced);
                }
                probes.extend(onto(&self.columns, sideways.tests(self.index)));
            }
            for (sideways, _) in &self.also {
                probes.extend(onto(&self.columns, sideways.tests(self.index)));
            }
            probes
        })
    }

    /// How good a row has to be for the top N above to still want it, as a test on one column.
    ///
    /// Empty for a scan with no top N above it, and empty until some instance of that top N has held
    /// a full set of candidates. It is one test at most, because only the first sort key says
    /// anything about a whole part. See [`crate::cutoff`].
    fn cutoff(&self) -> Vec<Probe> {
        let Some(cutoff) = self.cutoff.as_ref() else { return Vec::new() };
        let Some(test) = cutoff.probe(self.index) else { return Vec::new() };
        onto(&self.columns, vec![test])
    }

    /// Whether part `at` holds nothing anything above this scan could want.
    ///
    /// The two sets of tests are asked separately rather than joined into one, so that a scan with no
    /// top N above it does what it always did and a scan with one pays an allocation per part it
    /// hands back and nothing per part it walks past.
    fn ruled(&self, at: usize, probes: &[Probe], cutoff: &[Probe]) -> bool {
        let rows = self.table.rows();
        (!probes.is_empty() && rows.skips(at, probes))
            || (!cutoff.is_empty() && rows.skips(at, cutoff))
            || self.reduced_away(at)
    }

    /// The exact rows a join above handed down, and where part `at` starts, when there are some.
    ///
    /// The position is the row's place in the table, which is what a link calls a child `rid`, so
    /// the set can be asked about a part without reading anything.
    fn reduced(&self, at: usize) -> Option<(&Rids, u64)> {
        let rows = self.sideways.as_ref()?.rows(self.index)?;
        let first = u64::try_from(*self.offsets.get(at)?).ok()?;
        Some((rows, first))
    }

    /// Whether the exact rows hold nothing inside part `at`, so the part is never read.
    ///
    /// spec/graph/05-execution.md section 5.5. On a child stored in the order of its parent, which
    /// is `lineitem` against `orders`, a selective filter on the parent leaves most parts of the
    /// child with no member at all, and this is where they go.
    fn reduced_away(&self, at: usize) -> bool {
        let Some((rows, first)) = self.reduced(at) else { return false };
        let Some(len) =
            self.table.rows().chunk_len(at).ok().and_then(|len| u64::try_from(len).ok())
        else {
            return false;
        };
        len > 0 && !rows.any_between(first, first + len - 1)
    }

    /// The parts the statistics leave alive, by stripe, and how many rows they hold between them.
    ///
    /// This is the pruning the scan used to do while reading, asked before any worker is started, and
    /// it buys two things the read time version could not give. The instance count can be taken from
    /// the rows that are really coming rather than from the size of the table. And the work can be
    /// divided by where it is, which matters because a selective predicate on a file written in key
    /// order leaves its surviving parts next to each other: on ClickBench 39 the filter keeps about
    /// ninety of nine hundred and seventy four parts and they sit inside two stripes, so a morsel per
    /// stripe left fourteen of sixteen workers with nothing to do and the query was faster on one
    /// thread than on the whole machine.
    ///
    /// It asks in two passes because the two kinds of statistic cost very different amounts. The
    /// stripe bounds are in the directory and already in memory, so the first pass is sixteen
    /// comparisons and no reading at all. The sieves are a page per stripe off the disk, and that is
    /// the pass this skips whenever the bounds have already spread the work over enough stripes to
    /// keep every worker busy, because there the sieves would buy a better instance count at the
    /// price of doing serially what the workers were about to do at the same time. ClickBench 19 is
    /// the query that pays it: an equality on an identifier, where no stripe bound rules anything out
    /// and the sieves rule out all but nine parts, and doing them here made it half as fast.
    fn living(&self, threads: usize, weight: usize) -> (Vec<Live>, usize) {
        let (mut live, rows) = self.bounded();
        let working = live.iter().filter(|stripe| !stripe.parts.is_empty()).count();
        let probes = self.testing();
        if probes.is_empty() || !worth_sifting(working, threads, rows, weight) {
            return (live, rows);
        }
        let mut sifted = 0;
        for stripe in &mut live {
            stripe.parts.retain(|&at| !self.table.rows().skips(at, probes));
            stripe.rows =
                stripe.parts.iter().map(|&at| self.table.rows().chunk_len(at).unwrap_or(0)).sum();
            sifted += stripe.rows;
        }
        (live, sifted)
    }

    /// The first of those two passes, the one that reads nothing.
    ///
    /// A stripe nothing was pruned from takes its row count off the directory rather than by adding
    /// up its parts, which is the same answer without the walk. It is worth not doing rather than
    /// worth doing: this runs on the one thread every worker is waiting for, and a table in memory
    /// puts a hundred and twenty chunks in a row group, so on twenty million rows the walk was
    /// 19,532 lookups to arrive at 163 numbers the groups were already holding. It does not show up
    /// in a query time, and the reason to say so is that the same walk on a native file is sixty
    /// four parts to a stripe and nobody would have thought to look.
    fn bounded(&self) -> (Vec<Live>, usize) {
        let probes = self.testing();
        let mut rows = 0;
        let mut live = Vec::with_capacity(self.stripes.len());
        for (stripe, parts) in self.stripes.iter().enumerate() {
            if !probes.is_empty() && self.table.rows().stripe_skips(stripe, probes) {
                live.push(Live::default());
                continue;
            }
            let held = self.table.rows().stripe_rows(stripe);
            rows += held;
            live.push(Live { parts: parts.clone().collect(), rows: held });
        }
        (live, rows)
    }

    /// What this scan produces.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Connects this scan's part counts to the operator row that owns it.
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// The parts no morsel covers, which are the ones ruled out before any worker started.
    ///
    /// Counted here and not in [`Source::read`] because a part outside every run is never visited,
    /// so the walk has no chance to see it. Between this and the walk, every part of the table is
    /// counted exactly once, which is the invariant the test at the bottom of this file checks.
    fn unreached(&self, runs: &[Range<usize>]) {
        let Some(counters) = self.counters.as_ref() else { return };
        let covered: usize = runs.iter().map(|run| run.end.saturating_sub(run.start)).sum();
        for _ in 0..self.table.rows().chunk_count().saturating_sub(covered) {
            counters.part_pruned();
        }
    }
}

/// What one stripe has left in it after the statistics have been asked.
///
/// The two numbers travel together because they are answered together and because keeping them apart
/// is how the row count ends up being worked out twice: once to size the instances and once to cut
/// the runs. Empty parts and no rows is a stripe that was ruled out, which is a case the callers
/// test for rather than drop, since a run never crosses a stripe and the positions have to line up.
#[derive(Debug, Default)]
struct Live {
    /// The parts still worth reading, in order.
    parts: Vec<usize>,
    /// How many rows those parts hold.
    rows: usize,
}

/// Whether reading the sieves before the workers start is worth what it costs.
///
/// `working` is how many stripes the bounds left with anything in them and `rows` is what those hold.
/// The sieves would say which parts of those stripes really matter, which gives a truer instance
/// count and lets the work be cut where it is, and they cost a page read and a decode per stripe on
/// the one thread that is asking. That trade only pays when the bounds have left the work piled into
/// fewer stripes than there are workers to want it, because that is the case where handing out a
/// stripe apiece leaves most of the machine idle. When the work is already spread the workers find it
/// themselves, at the same time, each reading the sieves of the stripe it is in.
fn worth_sifting(working: usize, threads: usize, rows: usize, weight: usize) -> bool {
    working < threads.min(instances_for(rows, weight))
}

/// The live parts cut into the runs a morsel covers, by the rows they hold rather than by the stripe.
///
/// The unit used to be the stripe, because sixty four parts under one page is the read the disk wants
/// and a worker that owns the whole stripe owns the page, and a stripe was cut further only when there
/// were not enough stripes with work in them to go round. What that missed is that two stripes with
/// work in them are not two equal pieces of work. The statistics leave hundreds of live parts in one
/// and three in the next, both of them count as a stripe with work in it, and a morsel apiece hands
/// one worker a hundred times what it hands another. On ClickBench 39 that showed up as the slowest of
/// five instances taking 2.2 milliseconds against a mean of 0.96, which is more than half the query
/// spent waiting for one thread.
///
/// So the question is how many rows a stripe holds and not whether it holds any. A stripe holding no
/// more than one worker's share stays one run and reads its own page, which is every stripe of a table
/// nothing was pruned from, so the ownership the old rule was protecting is untouched in the case it
/// was written for. A stripe holding more than a share is cut, and it is cut into half shares rather
/// than into shares, because by then its page is being shared whatever happens and the only thing left
/// to play for is letting a worker that drew a slow piece be overtaken instead of waited for.
///
/// Dividing by rows is not the same as dividing by work, and when the statistics have piled the live
/// parts into fewer stripes than there are workers it is not even close. Equal rows go to every worker
/// and then a filter above the scan keeps six percent of them, and if the survivors sit together, which
/// on a file in key order they do, whichever worker holds them does all of the aggregate work above the
/// scan while the rest finish and stop. No division by rows can see that, because the rows are already
/// equal. What can survive it is having more pieces than workers, so that the worker holding the dense
/// piece is overtaken rather than waited for, and the piled case is exactly the case where cutting
/// finer is free: the stripes are already being shared, so the page ownership that argues for a whole
/// stripe has already been given up. A quarter of a share there, a whole share when there are as many
/// working stripes as workers.
///
/// Runs never cross a stripe either way. One that did would own two pages, which is the thing all of
/// this is avoiding.
fn runs_of(live: &[Live], instances: usize, rows: impl Fn(usize) -> usize) -> Vec<Range<usize>> {
    let total: usize = live.iter().map(|stripe| stripe.rows).sum();
    let working = live.iter().filter(|stripe| !stripe.parts.is_empty()).count();
    let share = total.div_ceil(instances.max(1)).max(1);
    // The biggest run allowed, and the size of the pieces an oversized stripe is cut into. They are
    // the same number in the piled case because there is nothing left to protect there.
    let (whole, piece) = if working >= instances {
        (share, share.div_ceil(2).max(1))
    } else {
        let piece = share.div_ceil(4).max(1);
        (piece, piece)
    };
    let mut runs: Vec<Range<usize>> = Vec::with_capacity(instances.saturating_mul(2));
    for stripe in live {
        let parts = &stripe.parts;
        let Some((&first, &last)) = parts.first().zip(parts.last()) else { continue };
        if stripe.rows <= whole {
            runs.push(first..last + 1);
            continue;
        }
        let mut taken = 0;
        let mut open = false;
        for &at in parts {
            let size = rows(at);
            // Closed before the part that would take it over rather than after, so a run is at most a
            // piece. A part bigger than a piece on its own is its own run, because a part is the
            // smallest thing a morsel can point at.
            if open && taken + size <= piece {
                if let Some(run) = runs.last_mut() {
                    run.end = at + 1;
                }
                taken += size;
            } else {
                runs.push(at..at + 1);
                taken = size;
                open = true;
            }
        }
    }
    runs
}

impl Source for Scan<'_> {
    fn morsel(&self) -> Option<Morsel> {
        let Some(spread) = self.spread.get() else {
            return self.chunks.take();
        };
        let index = spread.handout.number()?;
        let run = spread.runs.get(usize::try_from(index).unwrap_or(usize::MAX))?;
        let start = u64::try_from(run.start).unwrap_or(u64::MAX);
        let end = u64::try_from(run.end).unwrap_or(u64::MAX);
        Some(Morsel::new(index, start, end))
    }

    fn morsels(&self, threads: usize, weight: usize) -> Option<usize> {
        let chunks = self.chunks.total();
        // A table that says nothing about how its chunks are grouped has nothing to divide by, so
        // its chunks go out one at a time to whoever asks, which is what every table did before
        // there were row groups. The test is on the grouping and not on the format, because an in
        // memory table has row groups now and they are the same thing to everything below.
        if self.stripes.is_empty() {
            return Some(chunks);
        }
        let (live, rows) = self.living(threads, weight);
        // An instance is not free, so a scan asks for as many as the rows behind it can pay for
        // rather than for every worker the machine has. What one costs is a thread, and what it
        // buys is a share of the work, and `instances_for` is where those two are weighed. The
        // rows it is handed are the ones the zone maps leave rather than the ones the table holds,
        // because a query that reads a fifteenth of a file is a query the size of a fifteenth of a
        // file and asking for a worker per stripe of the whole of it is asking for fifteen threads
        // that start, find their stripe ruled out and stop.
        let instances = chunks.min(threads).min(instances_for(rows, weight));
        // A stripe per worker only divides the work when there are at least as many stripes as
        // workers. Below that it would leave workers with nothing, and the duplicate reads it
        // avoids are cheaper than the half of the machine it would cost, so a small table keeps
        // handing out parts. The reader is told what is coming before any of it starts, because a
        // page cache smaller than the number of workers in it evicts pages that are still in use.
        //
        // One worker takes stripes too, which page ownership on its own would not ask for, because
        // a morsel covering a run of parts is what lets [`Self::read`] walk past the parts the zone
        // maps rule out. A morsel covering one part cannot walk anywhere: it is drained the moment
        // that part is ruled out, so the scan has to hand the empty chunk up and be called again.
        if instances > 0 && self.stripes.len() >= instances {
            // Twice the workers rather than exactly the workers. The cache drops the page that has
            // been there longest, which with a slot per worker is always the page of whoever
            // entered their stripe first, which is the worker still in it. Room for the stripe
            // each worker has moved on to as well as the one it is in is what stops a straggler
            // reading its own page again, and it costs a page per worker per column.
            self.table.rows().keep_stripes(instances.saturating_mul(2));
            let runs = runs_of(&live, instances, |at| self.table.rows().chunk_len(at).unwrap_or(0));
            self.unreached(&runs);
            let _ = self.spread.set(Spread { handout: Handout::new(runs.len()), runs });
        }
        Some(instances)
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        // A part the zone maps have ruled out is never read, so its columns are never copied and
        // its rows are never handed to the filter above. It is also never handed up as an empty
        // chunk, which is what this loop is for: returning one costs a call into every operator in
        // the pipeline to carry nothing, and a selective query is mostly ruled out parts. On the
        // million row ClickBench file a point lookup rules out all but four of 974 parts and used
        // to push the other 970 through the filter and the projection one at a time, which was 1.7
        // of the 1.8 milliseconds it took to answer with no rows.
        //
        // Walking rather than returning is safe because a part that skips has nothing the caller
        // could want, so the only thing the old return said that this does not is how far the
        // morsel had got, and nothing outside asks that between one part and the next.
        let probes = self.testing();
        // Asked again for every part this hands back, rather than settled once the way `testing` is,
        // because the top N above fills it while this runs and it only ever tightens. Once per part
        // rather than once per call, since a call that walks a long run of ruled out parts is a call
        // during which the other workers are still reading and improving it. See [`crate::cutoff`].
        let cutoff = self.cutoff();
        let at = loop {
            let at = position(morsel);
            if at >= self.table.rows().chunk_count() || morsel.is_drained() {
                *out = Chunk::empty(&self.schema.types());
                return Ok(Progress::Done);
            }
            morsel.advance(1);
            if !self.ruled(at, probes, &cutoff) {
                break at;
            }
            self.skipped.fetch_add(1, Ordering::Relaxed);
            if let Some(counters) = &self.counters {
                counters.part_pruned();
            }
        };
        if let Some(counters) = &self.counters {
            counters.part_read();
        }
        if self.read_late(at, out)? || self.read_deferring(at, out)? {
            return Ok(more(morsel));
        }
        let projected: Vec<usize> = self.columns.iter().flatten().copied().collect();
        let read = self.table.rows().read(at, &projected)?;
        if self.columns.iter().all(Option::is_some) {
            *out = read;
            self.narrow_read(at, out)?;
            return Ok(more(morsel));
        }
        let mut held = Vec::with_capacity(self.columns.len());
        let mut real = 0;
        for column in &self.columns {
            if column.is_some() {
                held.push(read.column(real)?.clone());
                real += 1;
            } else {
                held.push(Vector::sequence(self.offsets[at], 1, read.len()));
            }
        }
        *out = Chunk::with_rows(held, read.len())?;
        self.narrow_read(at, out)?;
        Ok(more(morsel))
    }

    /// A pass over the rows for every step of the filter this scan took off the operator above it.
    ///
    /// Counted exactly the way [`crate::stream::Filter`] counts it, because it is that operator's
    /// work and the pipeline has to arrive at the same number whichever side of the scan boundary
    /// it is being done on. Zero for a scan with nothing pushed into it, which is a scan that only
    /// reads, and reading is the 1 the pipeline already counts.
    fn weight(&self) -> usize {
        self.pushed.as_ref().map_or(0, |pushed| pushed.predicate.passes())
    }
}

/// Whether a morsel the scan has just taken a part out of has another one in it.
fn more(morsel: &Morsel) -> Progress {
    if morsel.is_drained() { Progress::Done } else { Progress::More }
}

/// How many instances of a scan over a table cut into row groups are worth running.
///
/// An instance costs a worker, and a worker costs a lock, a push and a notify now that the pool
/// parks its threads rather than starting one per pipeline per run. It used to cost a thread
/// creation, about sixteen microseconds, and a sixteen way scan spent a quarter of a millisecond
/// before it read a row. What is left is the share of the work the instance takes, which is the
/// tension this function sits in: dividing a small table further buys less than the coordination
/// costs, whatever the coordination is.
///
/// Two slopes, and the larger wins. The first grows to eight and is what a small table uses, where
/// there are not many rows to divide and dividing them further buys less than the threads cost. The
/// second has no ceiling of its own and takes over past about half a million rows, where there is
/// enough work behind each instance that another one pays for itself. What actually stops it is the
/// pool, since [`Pipeline::degree`](rudb_pipeline::Pipeline::degree) clamps this to the threads the
/// database was given.
///
/// The numbers are measured rather than reasoned. Over the whole of ClickBench on the 999,975 row
/// `hits` sample, warm, with the cap forced to a fixed value: four is 261.3 ms, eight is 198.9,
/// sixteen is 188.1 and thirty two is 192.5. On the same suite over a 100,000 row sample: two is
/// 42.2 ms, four is 31.5, eight is 31.4 and sixteen is 35.9. So a million rows wants sixteen, a
/// hundred thousand wants four to eight, and both of those are what these two slopes give.
///
/// The old shape of this capped small tables at four and only grew past six hundred thousand rows,
/// on a measurement that said sixteen instances made the million row suite 16.6 percent slower.
/// That is no longer true of this engine and it is worth saying why rather than quietly changing
/// the constant: the per instance cost that measurement was paying has come down, so the cap moved
/// with it.
///
/// That comment used to end by saying these should be measured again once the thread stopped being
/// created per pipeline per run, because that was the last fixed cost in here and removing it would
/// take both slopes up. The thread is gone and the measurement was done, and the answer was no. On
/// the million row sample, halving both divisors is 196.3 ms against 199.8, and quartering them is
/// 196.5. On the hundred thousand row sample, halving them is 35.3 ms against 35.5 and quartering
/// them is 38.2. So dividing further is worth about a percent at a million rows and costs eight
/// percent at a hundred thousand, and the numbers stay where they are. What that says is that the
/// scan is no longer what limits how much of this machine a query uses, and the next thing to look
/// at is the operators above it.
///
/// The operators above it are what `weight` is. Both slopes were in rows, and a row is not what an
/// instance is worth, it is what the work behind the instance was being counted in. The two stop
/// agreeing the moment two queries spend different amounts on a row. ClickBench 39, 40 and 42 are
/// where that showed: each of them prunes to about twenty four thousand rows and each of them ran
/// the whole query on one thread of thirty two, because twenty four thousand is under the first
/// divisor. Twenty four thousand rows is small. Twenty four thousand rows of 39's aggregate, which
/// spends a hundred and eleven nanoseconds on each one grouping five columns with two wide strings
/// among them, is a millisecond of work on one core with thirty one idle.
///
/// So the small slope counts work and the large one still counts rows. That split is the whole of
/// the rule and it is deliberate. The small slope is the one that says a query is too small to
/// divide, and whether it is depends on what the query does with what it reads. The large slope is
/// the one that says how far a big query is worth dividing, and the measurements above say that is
/// sixteen at a million rows and that thirty two is worse, which is a fact about this machine and
/// not about the query, so nothing the operators say should move it.
///
/// Ordinary work is a weight of 1 and gives back exactly the rule that was measured. See
/// [`Stream::weight`](rudb_pipeline::Stream::weight) for what an operator is answering.
fn instances_for(rows: usize, weight: usize) -> usize {
    let small = rows.saturating_mul(weight.max(1)).div_ceil(25_000).min(8);
    let large = rows.div_ceil(62_500);
    small.max(large).max(1)
}

/// One row and no columns.
///
/// What `SELECT 1` sits on. It produces a chunk of width zero and length one exactly once, which is
/// the case `Chunk`'s stored row count exists for.
#[derive(Debug)]
pub(crate) struct Dummy {
    schema: Schema,
    one: Handout,
}

impl Dummy {
    pub(crate) fn new() -> Self {
        Self { schema: Schema::empty(), one: Handout::new(1) }
    }

    /// What this produces, which is no columns at all.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Source for Dummy {
    fn morsel(&self) -> Option<Morsel> {
        self.one.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.one.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = Chunk::with_rows(Vec::new(), 1)?;
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

/// Literal rows.
///
/// The expressions are evaluated once when the operator is built, over a one row chunk with no
/// columns, because a `VALUES` row in a bound plan is constants and folded arithmetic and cannot
/// refer to anything. Evaluating them lazily would buy nothing and would make an error in a literal
/// arrive on the first `next` rather than where the query says it is.
#[derive(Debug)]
pub(crate) struct Values {
    schema: Schema,
    chunks: Vec<Chunk>,
    handout: Handout,
}

impl Values {
    /// The rows of a [`Node::Values`](rudb_plan::Node::Values), already evaluated.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the column list, or anything the expressions report.
    /// The rows evaluated with the semantics of the query session.
    pub(crate) fn new(
        plan: &Plan,
        index: u32,
        columns: Slice,
        rows: Slice,
        session: &Session,
    ) -> Result<Self> {
        let time_zone = session.session_time_zone();
        let fields = plan.field_list(columns).to_vec();
        let schema = Schema::numbered(fields, index);
        let types = schema.types();
        let source = Schema::empty();
        let one = Chunk::with_rows(Vec::new(), 1)?;
        let mut down: Vec<Vec<Value>> = vec![Vec::new(); types.len()];
        for row in plan.row_list(rows) {
            let exprs: Vec<ExprRef> = plan.expr_list(*row).to_vec();
            if exprs.len() != types.len() {
                return Err(Error::internal(format!(
                    "a VALUES row of {} expressions in a {} column list",
                    exprs.len(),
                    types.len()
                )));
            }
            let evaluated = evaluate_all_in_time_zone(plan, &exprs, &source, &one, time_zone)?;
            for (position, vector) in evaluated.iter().enumerate() {
                down[position].push(vector.value_at(0));
            }
        }
        let total = down.first().map_or(0, Vec::len);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < total {
            let end = (start + VECTOR_SIZE).min(total);
            let mut built = Vec::with_capacity(types.len());
            for (position, ty) in types.iter().enumerate() {
                built.push(Vector::from_values(ty.clone(), &down[position][start..end])?);
            }
            chunks.push(Chunk::with_rows(built, end - start)?);
            start = end;
        }
        let handout = Handout::new(chunks.len());
        Ok(Self { schema, chunks, handout })
    }

    /// What these rows are.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

/// A table function that produces a run of integers.
///
/// The arguments are evaluated once when the operator is built, the same way a `VALUES` row is and
/// for the same reason: they are constants by the time they are here, since a table function that
/// can see a row is `LATERAL` and does not bind to this node.
///
/// The values are produced a chunk at a time rather than all at once. `range(100000000)` is a
/// hundred million rows and a corpus that writes it means it, so materializing the whole run into
/// a `Vec` before the first chunk comes out would be eight hundred megabytes for a query whose
/// answer is one number.
/// A morsel here is a run of positions in the sequence, several chunks long, because the rows are
/// worked out rather than read and handing out a morsel per chunk would be more counter traffic than
/// arithmetic. Sixteen chunks is small enough that a hundred million rows is still six thousand
/// units for a scheduler to balance and large enough that the handout is not the cost.
#[derive(Debug)]
pub(crate) struct Series {
    schema: Schema,
    /// The first value, which is the value at position zero.
    start: i64,
    step: i64,
    /// How many values there are, which `series_length` worked out once.
    rows: u64,
    /// BIGINT, or the moment type of a series of moments.
    ty: LogicalType,
    /// The moments of a series whose step has months in it, which cannot be worked out from a
    /// position and so are walked once and kept.
    listed: Option<Arc<[i64]>>,
    morsels: AtomicU64,
}

/// How many positions one morsel of a series covers.
const RUN: u64 = 16 * VECTOR_SIZE as u64;

impl Series {
    /// The rows of a [`Node::TableFunction`](rudb_plan::Node::TableFunction).
    ///
    /// A null in any argument gives no rows at all, which is DuckDB's answer and is not the same
    /// as an error. The three defaults are the three that make a one argument call mean what
    /// everybody writes it to mean, which is zero up to the number.
    ///
    /// # Errors
    ///
    /// Whatever evaluating an argument reports, and a step of zero.
    pub(crate) fn new(plan: &Plan, index: u32, function: &str, args: Slice) -> Result<Self> {
        let Some(function) = TableFunction::lookup(function) else {
            return Err(Error::internal(format!("a plan with a table function called {function}")));
        };
        let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
        let source = Schema::empty();
        let one = Chunk::with_rows(Vec::new(), 1)?;
        let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
        if let [start, stop, step] = evaluated.as_slice() {
            if *step.logical_type() == LogicalType::Interval {
                let ty = start.logical_type().clone();
                let schema = Schema::numbered(vec![Field::new(function.name(), ty.clone())], index);
                let empty = Self { ty, ..Self::empty(schema.clone()) };
                let values = (start.value_at(0), stop.value_at(0), step.value_at(0));
                let Some(stepping) = moments(function, &values.0, &values.1, &values.2)? else {
                    return Ok(empty);
                };
                let rows = u64::try_from(stepping.len()).unwrap_or(u64::MAX);
                return Ok(match stepping {
                    Stepping::Even { start, step, .. } => Self { start, step, rows, ..empty },
                    Stepping::Listed(stamps) => Self { rows, listed: Some(stamps.into()), ..empty },
                });
            }
        }
        let fields = vec![Field::new(function.name(), LogicalType::BigInt)];
        let schema = Schema::numbered(fields, index);
        let mut given = Vec::with_capacity(evaluated.len());
        for vector in &evaluated {
            match vector.value_at(0) {
                Value::Null => return Ok(Self::empty(schema)),
                Value::BigInt(n) => given.push(n),
                other => {
                    return Err(Error::internal(format!(
                        "a table function argument bound as BIGINT arrived as {other}"
                    )));
                }
            }
        }
        let (start, stop, step) = match given.as_slice() {
            [stop] => (0, *stop, 1),
            [start, stop] => (*start, *stop, 1),
            [start, stop, step] => (*start, *stop, *step),
            _ => {
                return Err(Error::internal(format!(
                    "{}() bound with {} arguments",
                    function.name(),
                    given.len()
                )));
            }
        };
        let rows = u64::try_from(series_length(function, start, stop, step)?).unwrap_or(u64::MAX);
        Ok(Self { start, step, rows, ..Self::empty(schema) })
    }

    fn empty(schema: Schema) -> Self {
        Self {
            schema,
            start: 0,
            step: 1,
            rows: 0,
            ty: LogicalType::BigInt,
            listed: None,
            morsels: AtomicU64::new(0),
        }
    }

    /// What this produces, which is one column named after the function.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The value at a position in the sequence.
    ///
    /// Worked out rather than carried, because a thread that is handed the tenth morsel has not
    /// counted its way to it and never will. Inside a chunk the step is still added a row at a time,
    /// which is what keeps the answers the same as the loop that used to be here.
    fn value_at(&self, position: u64) -> i64 {
        let steps = i64::try_from(position).unwrap_or(i64::MAX);
        self.start.saturating_add(self.step.saturating_mul(steps))
    }
}

impl Source for Series {
    fn morsel(&self) -> Option<Morsel> {
        let index = self.morsels.fetch_add(1, Ordering::Relaxed);
        let start = index.saturating_mul(RUN);
        (start < self.rows)
            .then(|| Morsel::new(index, start, self.rows.min(start.saturating_add(RUN))))
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(usize::try_from(self.rows.div_ceil(RUN)).unwrap_or(usize::MAX))
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let count = usize::try_from(morsel.remaining()).unwrap_or(usize::MAX).min(VECTOR_SIZE);
        if count == 0 {
            *out = Chunk::empty(std::slice::from_ref(&self.ty));
            return Ok(Progress::Done);
        }
        // The loop is over `i64` rather than over `Value`, and the vector is built out of the run
        // it fills rather than out of a list of tagged values that would have to be read back one
        // at a time to find the run again. `range()` is the source every microbenchmark in
        // `rudb-bench` reads from, so a chunk of it costing a `Value` a row would be measuring the
        // generator instead of what is downstream of it.
        let counted = if let Some(listed) = &self.listed {
            let from = usize::try_from(morsel.cursor()).unwrap_or(usize::MAX);
            let Some(run) = listed.get(from..from.saturating_add(count)) else {
                return Err(Error::internal("a morsel past the end of a series of moments"));
            };
            run.to_vec()
        } else {
            let mut at = self.value_at(morsel.cursor());
            let mut counted = Vec::with_capacity(count);
            for _ in 0..count {
                counted.push(at);
                at = at.saturating_add(self.step);
            }
            counted
        };
        morsel.advance(u64::try_from(count).unwrap_or(u64::MAX));
        let vector = Vector::flat(self.ty.clone(), Data::Int64(counted.into()))?;
        *out = Chunk::with_rows(vec![vector], count)?;
        Ok(if morsel.is_drained() { Progress::Done } else { Progress::More })
    }
}

/// The moments a `range` or `generate_series` call over dates or timestamps gives, or `None` when an
/// argument is null, which is no rows at all.
///
/// The bounds are checked here and not in the kernel because the table form refuses them in its
/// own words, as binder errors, and refuses a zero interval where the list form answers empty.
///
/// # Errors
///
/// An infinite bound, a zero interval, one with mixed signs, and a series past 2^32 moments.
pub(crate) fn moments(
    function: TableFunction,
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<Option<Stepping>> {
    let moment = |value: &Value| match value {
        Value::Timestamp(stamp) | Value::TimestampTz(stamp) => Ok(Some(*stamp)),
        Value::Null => Ok(None),
        other => Err(Error::internal(format!("a range of moments from a {other}"))),
    };
    let (Some(start), Some(stop), Value::Interval { months, days, micros }) =
        (moment(start)?, moment(stop)?, step)
    else {
        return Ok(None);
    };
    if [start, stop].iter().any(|&stamp| stamp == i64::MAX || stamp == -i64::MAX) {
        return Err(Error::binder("RANGE with infinite bounds is not supported"));
    }
    let forward = *months > 0 || *days > 0 || *micros > 0;
    let backward = *months < 0 || *days < 0 || *micros < 0;
    if !forward && !backward {
        return Err(Error::binder("interval cannot be 0!"));
    }
    if forward && backward {
        return Err(Error::binder(
            "RANGE with composite interval that has mixed signs is not supported",
        ));
    }
    moment_steps(function.inclusive(), start, stop, (*months, *days, *micros)).map(Some)
}

/// A scan of one or more files, Parquet or CSV.
///
/// The files are named by the arguments, which the binder already expanded: a pattern was walked
/// there and a name that is not a pattern was checked there, so what arrives is a list of names that
/// existed when the statement was bound. They are opened here rather than being carried from the
/// binder, because binding and running are separated by however long a prepared statement lives and
/// a plan that held open descriptors would hold them for all of that.
///
/// One at a time, in the order the list gives, which is the order the rows come out in. Opening all
/// of them up front would mean a directory of ten thousand files costing ten thousand descriptors
/// before the first row, and closing each one at its end is what makes a scan of a whole directory
/// cost one.
///
/// The plan's column list is resolved against each file's by name, which is the same thing [`Scan`]
/// does against a catalog table and for the same reason. Today the binder projects every column in
/// order, so the mapping is the identity, and the moment projection pushdown makes the plan's list a
/// subset the reader reads a subset. That is the difference `spec/engine/05-scan.md` section 5.6
/// describes between reading two columns of ClickBench and reading a hundred and five.
///
/// The first file decides the types and every file after it is cast to them, which is DuckDB's rule
/// and was measured: a second file holding `'5'` where the first holds an `INTEGER` reads as 5, and
/// one holding `'txt'` is a conversion error naming the file it came from.
///
/// That is the Parquet rule and CSV does not follow it. A Parquet file states its schema, so there
/// is a first file's word to take, and a CSV file states nothing, so the binder sniffed all of them
/// and combined the answers. What arrives here is that combined answer, and each CSV file is told it
/// as it is opened rather than being allowed to use its own sample, which is what keeps a file that
/// happens to hold nothing but whole numbers from handing up BIGINT into a stream that is DOUBLE.
///
/// A morsel is one row group of one Parquet file, or one range of bytes of one CSV file.
///
/// The row group is what the format stores and what a reader can be positioned at without having
/// read what came before it, so it is the smallest unit two threads can take without one of them
/// waiting on the other. A CSV file cannot be positioned, because nothing in it says where a row
/// begins until every byte before it has been parsed, so its ranges guess where their first row
/// starts and check the guess against where the range before ended. [`rudb_csv::split`] is where
/// that is done. A CSV file is one morsel when it is short, when the query has one thread, and when
/// the query wants `file_row_number`, since a range does not know how many rows came before it.
///
/// The files are cut into morsels one file at a time rather than all at once. Cutting a file means
/// reading its footer, and a directory of ten thousand files would be ten thousand footers read
/// before the first row came out, which is what the paragraph above about descriptors is about and
/// is the same answer.
///
/// Each morsel carries its own reader, and for Parquet those readers share one open file and one
/// parsed footer through [`Reader::split`]. So the scan holds no lock across a read, which is the
/// whole point: the version of this before #486 had one morsel, one reader and a mutex around it,
/// and a second thread asking for work got none.
///
/// `sideways` is what a join above this scan worked out about the key it is going to look these
/// rows up by, which is the same handoff [`Scan`] takes and for the same reason. It arrives here
/// rather than in the plan because it is not known until the join's other side has finished. Only
/// the filter tier is read: the range tier would have to reach the row group bounds, and those are
/// asked before the build side has run.
#[derive(Debug)]
pub(crate) struct FileScan<'a> {
    function: TableFunction,
    paths: Vec<String>,
    given: Given,
    wanted: Vec<Field>,
    /// Whether the last column the scan produces is the row's ordinal inside its own file.
    ///
    /// `file_row_number=True`, which is a column no file holds and the scan counts. It is last
    /// because the binder puts it last, and it is a flag rather than a position because everything
    /// else here indexes [`Self::wanted`] and that list is the file's columns only.
    numbered: bool,
    schema: Schema,
    /// The comparisons a row group's bounds can be checked against before it is handed out.
    ///
    /// Written in terms of this scan's own output positions, because that is what the filter above
    /// it is written in terms of and what the builder can read without knowing which file is open.
    /// [`Self::advance`] turns them into the file's column numbers, once per file, since two files
    /// of one glob are allowed to hold the same columns in a different order.
    ///
    /// Empty when there is no filter above the scan, when the filter has no conjunct a bound can
    /// answer, or when the source is a CSV, and empty means every row group is handed out, which is
    /// what every scan did before this existed.
    tests: Vec<(usize, Op, Bound)>,
    /// How far through the file list the cutting has got, and the file it is in the middle of.
    cutting: Mutex<Cutting>,
    /// What each morsel handed out covers, by [`Morsel::index`].
    ///
    /// The outer lock is held for a lookup and a clone of one handle, and the read that follows
    /// holds the inner one, so two threads reading two row groups never wait for each other. The
    /// entries stay after their morsel is drained, because they are three words and a dropped
    /// reader once the rows are out and because a driver is allowed to ask again.
    open: Mutex<HashMap<u64, Arc<Mutex<Piece>>>>,
    counters: Option<Arc<Counters>>,
    /// The runtime filter of the join this scan drives, empty for a scan that drives no join.
    sideways: Option<Arc<Sideways<'a>>>,
    /// Which table index this scan's columns bind against, which is how the runtime filter knows
    /// whether it is about one of them.
    index: u32,
    /// Whether the runtime filter has been earning the hash it costs.
    paying: Paying,
}

/// How many rows a scan aims to put in one morsel.
///
/// A morsel is the unit of work a thread takes, so it decides two things at once: how many threads
/// can be busy at all, and how evenly the last round of work divides among them. A row group is the
/// obvious unit and it is the wrong size for both. DuckDB writes a hundred and twenty two thousand
/// rows into one, so the million row ClickBench file has nine, and nine pieces of work is nine busy
/// threads on a machine with thirty two and one straggler deciding when everybody is finished.
///
/// Thirty two thousand is measured rather than picked. The same suite over copies of that file
/// written with different row group sizes runs in 898 ms on nine groups, 749 on thirty one and 768
/// on sixty one, all at eight threads, and at thirty two threads the nine group file is slower than
/// it was at eight while the thirty one group file is faster again at 676.
///
/// Whether a row group is cut at all is [`morsel_rows`], which asks the file how long its pages are
/// first, because a morsel cannot start inside a page for free.
const MORSEL_ROWS: usize = 32_768;

/// The rows a run of whole row groups is gathered up to, or zero when groups are being cut instead
/// or the sink asked for nothing.
///
/// A file written with small row groups is as common as one written with large ones: the ClickBench
/// ten million row sample is 1,203 groups of about eight thousand rows, and anything written by a
/// streaming job or a Spark stage with many small tasks looks the same. Handed out one group at a
/// time, every morsel pays a split of the reader, a registration and a hand over at the sink, and a
/// load into a native file pays far more, because that sink never lets a stripe span two morsels.
/// It writes eight thousand row stripes one at a time behind the writer's lock, and loading the
/// sample took 73.6 s against 22.2 s for the same rows in 81 groups, with 1,203 writes waiting a
/// summed 961 s. So a sink that pays for a boundary asks through [`Source::gather`] and the groups
/// are gathered until the next would take the run past what it asked for, and never so far that
/// there are fewer morsels than threads. A group that large or larger is handed out alone.
///
/// A query over the file asks for nothing and is cut as it always was. Gathering the same sample
/// for queries made the ClickBench set 6 to 14% slower on 32 threads, measured and not explained
/// yet, though fewer and larger morsels leaving threads idle at the end of a scan is the suspect.
fn gather_rows(reader: &FileReader, cut: usize, threads: usize, asked: usize) -> usize {
    let FileReader::Parquet(parquet) = reader else { return 0 };
    if cut != 0 || asked == 0 {
        return 0;
    }
    let total = parquet
        .metadata()
        .row_groups
        .iter()
        .map(|group| usize::try_from(group.rows).unwrap_or(usize::MAX))
        .fold(0_usize, usize::saturating_add);
    gather_target(total, threads, asked)
}

/// The arithmetic of [`gather_rows`]: a share of the file for every thread, and no more than the
/// sink asked for.
fn gather_target(total: usize, threads: usize, asked: usize) -> usize {
    (total / threads.max(1)).min(asked)
}

/// How many row groups the run starting with a group of `first` rows takes, `later` being the rows
/// of the groups after it that may join.
///
/// Always at least the first. The next joins only if the run stays within `gather` rows with it, so
/// a run never grows past the target by more than its first group.
fn gathered(first: usize, later: impl Iterator<Item = usize>, gather: usize) -> usize {
    let mut total = first;
    let mut taken = 1;
    for rows in later {
        match total.checked_add(rows) {
            Some(next) if next <= gather => {
                total = next;
                taken += 1;
            }
            _ => break,
        }
    }
    taken
}

/// How many times what a morsel wastes it has to read before the cutting is worth doing.
///
/// A morsel that starts inside a page pays for that page decoded twice, once by the morsel that ends
/// in it and once by the morsel that starts in it. The columns of a row group do not break their
/// pages in the same places, so a boundary costs one page of every column read, and holding a
/// morsel's first pages against the bytes it goes on to read is what says whether that is a rounding
/// error or the whole read.
///
/// Four is the smallest number that still refuses the file DuckDB writes, where the first page of a
/// chunk is the whole chunk and the waste is not a quarter but everything. A page a quarter of a
/// morsel long is measured to be worth cutting anyway: the ClickBench suite over a copy of the file
/// written with 64 KB pages runs in 770 ms at thirty two threads against 909 uncut.
///
/// Asking in bytes rather than in rows matters, and asking in rows is what this did first and got
/// wrong. A column with four distinct values packs a whole row group into one page of two bit
/// dictionary indices, so the longest page of a file measured in rows is always the cheapest column
/// in it, and a file of small pages was refused because of the one column that cost nothing to read.
const FINE: u64 = 4;

/// How many rows one morsel of a row group covers, or zero to hand out whole row groups.
///
/// Two things have to be true before a row group is worth cutting, and on the files this engine is
/// measured against neither of them is.
///
/// There have to be threads with nothing to do. A file of nine row groups already keeps eight
/// threads busy, so cutting it costs the cutting and buys nothing, and the cut is made exactly fine
/// enough to give every thread a piece rather than as fine as it will go.
///
/// The pages have to be small. A morsel steps over whole pages for free but decodes the page its
/// first row sits in, so a page as long as a row group means a group cut four ways is a group
/// decoded four times. DuckDB writes exactly that, one page per column chunk, and cutting its files
/// anyway is a measured disaster: the suite at one thread went from 2.9 seconds to 5.9 with the scan
/// reading, decompressing and decoding four times the bytes for the same answers.
fn morsel_rows(reader: &FileReader, groups: usize, threads: usize) -> usize {
    let FileReader::Parquet(parquet) = reader else { return 0 };
    if groups == 0 || threads <= groups {
        return 0;
    }
    let page = parquet.page_bytes().unwrap_or(u64::MAX);
    cut_rows(page, parquet.chunk_bytes(), group_rows(parquet, 0), groups, threads)
}

/// The arithmetic of [`morsel_rows`], with the file already asked its questions.
///
/// `page` is what a morsel reads before it reads a row it wants, `whole` what reading the group it
/// is part of costs, and `rows` how many rows that group holds, all over the projected columns.
fn cut_rows(page: u64, whole: u64, rows: usize, groups: usize, threads: usize) -> usize {
    if groups == 0 || threads <= groups || rows == 0 || page == 0 {
        return 0;
    }
    let cut = rows.div_ceil(threads.div_ceil(groups)).max(MORSEL_ROWS);
    let taking = u64::try_from(cut.min(rows)).unwrap_or(u64::MAX);
    let each = whole / u64::try_from(rows).unwrap_or(u64::MAX) * taking;
    if page.saturating_mul(FINE) > each { 0 } else { cut }
}

/// The rows of the next morsel of a row group of `rows` rows, `part` of which are handed out.
///
/// Even pieces rather than full ones and a remainder, because the remainder is the piece everybody
/// else waits for. A group of a hundred and twenty three thousand rows is four morsels of thirty one
/// thousand rather than three of thirty two thousand and one of twenty five.
///
/// An empty range means the group is done, and a group of no rows is done straight away, which is
/// what stops a file with an empty row group in it from being cut forever.
fn next_piece(rows: usize, part: usize, target: usize) -> Range<usize> {
    let each = rows.div_ceil(parts(rows, target));
    let upto = part.saturating_add(each).min(rows);
    part.min(upto)..upto
}

/// How many morsels a row group of `rows` rows is cut into, which is one when `target` is zero.
fn parts(rows: usize, target: usize) -> usize {
    if target == 0 {
        return 1;
    }
    rows.div_ceil(target).max(1)
}

/// How many rows the row group at `at` holds, or none if it is not a group this file has.
fn group_rows(reader: &Reader, at: usize) -> usize {
    reader
        .metadata()
        .row_groups
        .get(at)
        .map_or(0, |group| usize::try_from(group.rows).unwrap_or(usize::MAX))
}

/// Decides how finely the file being cut is worth cutting, and how many morsels that comes to.
///
/// Called when a file is opened and again when the scheduler says how many threads it has, which
/// happens in that order on the first file and the other way round on the rest. Both are cheap: the
/// page size is read once per file and the rest is arithmetic over the footer.
fn aim(cutting: &mut Cutting) {
    let Some(reader) = cutting.reader.as_ref() else { return };
    cutting.cut = morsel_rows(reader, cutting.groups, cutting.threads);
    cutting.gather = gather_rows(reader, cutting.cut, cutting.threads, cutting.asked);
    cutting.pieces = pieces(reader, cutting.cut, cutting.gather);
}

/// How many morsels a whole file comes to, which is what [`Source::morsels`] answers with.
fn pieces(reader: &FileReader, target: usize, gather: usize) -> usize {
    let FileReader::Parquet(reader) = reader else { return 1 };
    let rows: Vec<usize> = reader
        .metadata()
        .row_groups
        .iter()
        .map(|group| usize::try_from(group.rows).unwrap_or(usize::MAX))
        .collect();
    morsels_of(&rows, target, gather)
}

/// How many morsels groups of these sizes come to, cut to `target` rows or gathered up to `gather`.
fn morsels_of(rows: &[usize], target: usize, gather: usize) -> usize {
    if gather == 0 {
        return rows.iter().map(|&group| parts(group, target)).sum::<usize>().max(1);
    }
    let mut at = 0;
    let mut morsels = 0;
    while let Some(&first) = rows.get(at) {
        at += gathered(first, rows[at + 1..].iter().copied(), gather);
        morsels += 1;
    }
    morsels.max(1)
}

/// Where the cutting has got to.
///
/// One file's worth of morsels is cut at a time, and the reader the Parquet splits come off is kept
/// for as long as that file has row groups left to hand out.
#[derive(Debug)]
struct Cutting {
    /// How many files have been opened, which is the next one to open.
    at: usize,
    /// The reader the file being cut is read through, `None` between files.
    reader: Option<FileReader>,
    /// The next row group of that file to hand out, and one past its last.
    group: usize,
    groups: usize,
    /// How many rows of that row group have been handed out already.
    ///
    /// A row group is cut into several morsels when it is large enough to be worth cutting, so the
    /// cutting sits inside a group as well as between them. Zero whenever the next morsel starts a
    /// group, which is every morsel of a file whose groups are small.
    part: usize,
    /// How many rows one morsel of a row group of this file covers, or zero for whole row groups.
    ///
    /// Worked out when the file is opened and again when the scheduler says how many threads it has,
    /// because whether cutting a group pays depends on the page size the writer chose and on there
    /// being a thread with nothing else to do.
    cut: usize,
    /// How many rows a run of small whole row groups is gathered up to, or zero when groups are
    /// being cut rather than gathered. See [`gather_rows`].
    gather: usize,
    /// How many rows the sink asked a morsel to hold, or zero when it asked for nothing, which is
    /// every sink but a load. Kept across files, since it is asked once and every file is aimed.
    asked: usize,
    /// How many threads the scheduler said it would lend, which is one until it says otherwise.
    ///
    /// [`Source::morsels`] is asked before any instance starts and is given the ceiling, so this is
    /// set by the time the cutting matters. One until then, which means no cutting, because a scan
    /// nobody told is a scan that should not be inventing work.
    threads: usize,
    /// How many morsels the whole of the file being cut comes to.
    ///
    /// Worked out once when the file is opened, because [`Source::morsels`] is asked before any of
    /// them is handed out and it is asked to decide how many instances of a pipeline to build.
    pieces: usize,
    /// [`FileScan::tests`] against the column numbers of the file being cut.
    skipping: Vec<Test>,
    /// How many row groups the bounds have ruled out so far, over every file of the scan.
    ///
    /// Nothing downstream needs this. It is here because a pruning that silently stops working
    /// costs time and nothing else, so the tests read it to prove groups are actually being
    /// skipped rather than read and filtered.
    skipped: usize,
    /// The ordinal in that file of the first row of the next morsel, which is what
    /// `file_row_number` counts from and what keeps that column right whatever order the morsels
    /// are read in.
    row: i64,
    /// How many morsels have been handed out, which is the next one's index.
    given: u64,
    /// The CSV file being cut, once it has been cut into ranges, and the next range to hand out.
    ///
    /// The reader it was opened with is inside it, so [`Cutting::reader`] is `None` while it is set.
    split: Option<(Arc<Split>, usize)>,
}

/// What one morsel covers, and the reader open on it.
#[derive(Debug)]
struct Piece {
    /// Which of the scan's paths, so that a column that will not cast names the file it came from.
    file: usize,
    /// The reader, taken away once it has no more chunks in it.
    reader: Option<FileReader>,
    /// What went wrong cutting this morsel, if anything.
    ///
    /// [`Source::morsel`] hands back an `Option` and has nowhere to put an error, and a file that
    /// cannot be opened half way through a scan has to be reported rather than read past. So the
    /// handout queues a morsel that covers nothing and carries the error, and the read reports it.
    failure: Option<Error>,
    /// The ordinal in the file of the next row this morsel produces.
    row: i64,
}

impl<'a> FileScan<'a> {
    /// The rows of a `read_parquet` or `read_csv` call, over every file it names.
    ///
    /// # Errors
    ///
    /// If the first file is gone or unreadable since it was bound, or if it no longer has a column
    /// the plan asked for, which is what a file replaced between binding and running looks like.
    ///
    /// All but the last of these are the plan and the fields of one node of it, and bundling them
    /// into a struct on the way in would only spell the same node a second way.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        plan: &Plan,
        index: u32,
        function: TableFunction,
        args: Slice,
        options: Slice,
        settings: Slice,
        columns: Slice,
        tests: Vec<(usize, Op, Bound)>,
        sideways: Option<Arc<Sideways<'a>>>,
    ) -> Result<Self> {
        let paths = file_arguments(plan, args, function)?;
        let given = csv_options(plan, options, settings)?;
        let produced = plan.field_list(columns).to_vec();
        // The binder puts the counted column last and nothing between here and there reorders a
        // scan's columns, so the flag is whether the last one is it. Pruning can drop it, in which
        // case there is nothing to count, and pruning can drop everything else, in which case the
        // file is opened for its row count and no column of it is read.
        let numbered = produced.last().is_some_and(|field| field.name == FILE_ROW_NUMBER);
        let wanted =
            if numbered { produced[..produced.len() - 1].to_vec() } else { produced.clone() };
        let scan = Self {
            function,
            paths,
            given,
            wanted,
            numbered,
            schema: Schema::numbered(produced, index),
            tests,
            cutting: Mutex::new(Cutting {
                at: 0,
                reader: None,
                group: 0,
                groups: 0,
                part: 0,
                cut: 0,
                gather: 0,
                asked: 0,
                threads: 1,
                pieces: 1,
                skipping: Vec::new(),
                skipped: 0,
                row: 0,
                given: 0,
                split: None,
            }),
            open: Mutex::new(HashMap::new()),
            counters: None,
            sideways,
            index,
            paying: Paying::default(),
        };
        // The first file is opened now rather than on the first read, so that a file that has gone
        // missing since binding is reported where a caller is still asking a question about this
        // scan rather than in the middle of a result.
        {
            let mut cutting = scan.cutting.lock().map_err(poisoned)?;
            scan.advance(&mut cutting)?;
        }
        Ok(scan)
    }

    /// Drops the rows of one chunk that a join above this scan cannot hold a match for.
    ///
    /// The same test [`Scan::sift`] makes and the same argument for it, applied where the rows come
    /// out of a file rather than out of a stored table. One hash of one column and one cache line
    /// touched per row, against a filter holding the keys the build side turned out to have.
    ///
    /// Nothing at all for a scan with no join above it, which is two loads and a branch per chunk,
    /// and nothing either once [`Paying`] has decided the filter is not turning enough rows away to
    /// be worth hashing for.
    ///
    /// # Errors
    ///
    /// Whatever narrowing the chunk to the rows that survived raises.
    fn sift(&self, chunk: &mut Chunk) -> Result<()> {
        let Some(sideways) = self.sideways.as_ref() else { return Ok(()) };
        if !self.paying.worth() {
            return Ok(());
        }
        // Keys close together come as a bitmap in place of the filter, see `Found::domain`.
        if let Some((at, domain)) = sideways.domain(self.index) {
            let Ok(column) = chunk.column(at) else { return Ok(()) };
            let rows = chunk.len();
            let kept = domain.keep(column, rows, &mut Vec::new());
            self.paying.saw(rows, kept.len());
            if kept.len() < rows {
                let whole = std::mem::replace(chunk, Chunk::empty(&[]));
                *chunk = whole.select(&Selection::from_indices(kept))?;
            }
            return Ok(());
        }
        let Some((at, filter)) = sideways.sifting(self.index) else { return Ok(()) };
        let Ok(column) = chunk.column(at) else { return Ok(()) };
        let rows = chunk.len();
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(column), rows, &mut hashes, Across::TwoInputs);
        // The whole chunk asked at once rather than a row at a time inside the selection, because
        // the filter is larger than the cache and a row of it is a trip to memory the core can only
        // overlap with the next row's if nothing in between branches on the answer.
        let mut held = Vec::new();
        filter.holds_run(&hashes, &mut held);
        let kept = Selection::from_predicate(rows, |row| held[row]);
        self.paying.saw(rows, kept.len());
        if kept.len() == rows {
            return Ok(());
        }
        let whole = std::mem::replace(chunk, Chunk::empty(&[]));
        *chunk = whole.select(&kept)?;
        Ok(())
    }

    /// Connects this source's file counters to the operator row that owns it.
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// What this scan produces, in the types the first file settled on.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Opens the next file and projects it, or leaves the reader empty at the end of the list.
    fn advance(&self, cutting: &mut Cutting) -> Result<()> {
        cutting.reader = None;
        cutting.split = None;
        cutting.row = 0;
        cutting.group = 0;
        cutting.groups = 0;
        cutting.part = 0;
        cutting.cut = 0;
        cutting.gather = 0;
        cutting.pieces = 1;
        cutting.skipping = Vec::new();
        let Some(path) = self.paths.get(cutting.at) else { return Ok(()) };
        let mut reader = FileReader::open(self.function, path, self.given)?;
        let first = if cutting.at == 0 { None } else { self.paths.first().map(String::as_str) };
        let held = positions(self.function, &self.wanted, &reader.fields(), path, first)?;
        reader.project(&held)?;
        reader.settle(&self.wanted)?;
        cutting.groups = reader.row_groups();
        cutting.skipping = self
            .tests
            .iter()
            .filter_map(|(at, op, value)| {
                Some(Test { column: *held.get(*at)?, op: *op, value: value.clone() })
            })
            .collect();
        cutting.reader = Some(reader);
        cutting.at += 1;
        aim(cutting);
        self.divide(cutting);
        Ok(())
    }

    /// Cuts the CSV file being read into ranges, when it is long enough and there are threads to
    /// read them on.
    ///
    /// Not when the query wants `file_row_number`, since a range has no way to know how many rows
    /// came before it short of reading them, and the number is the one thing the column is for.
    fn divide(&self, cutting: &mut Cutting) {
        if cutting.threads <= 1 || self.numbered {
            return;
        }
        let size = rudb_csv::split::size();
        match cutting.reader.take() {
            Some(FileReader::Csv(reader)) if reader.ranges(size) > 1 => {
                let ranges = reader.ranges(size);
                let split = Split::new(reader, ranges);
                cutting.pieces = split.ranges();
                cutting.split = Some((Arc::new(split), 0));
            }
            other => cutting.reader = other,
        }
    }

    /// The next morsel's worth of the file being cut, or `None` when that file has none left.
    ///
    /// A Parquet file gives one per row group and takes a split of the reader it was opened with. A
    /// CSV file that was cut into ranges gives one per range, and one that was not gives one, which
    /// takes the reader itself.
    fn cut(&self, cutting: &mut Cutting) -> Result<Option<Piece>> {
        let file = cutting.at.saturating_sub(1);
        if let Some((split, next)) = cutting.split.as_mut() {
            if *next == split.ranges() {
                return Ok(None);
            }
            let part = split.part(*next);
            *next += 1;
            return Ok(Some(Piece {
                file,
                reader: Some(FileReader::Part(part)),
                failure: None,
                row: 0,
            }));
        }
        if let Some(FileReader::Parquet(reader)) = cutting.reader.as_ref() {
            // Row groups the filter above this scan has already ruled out are stepped over here
            // rather than handed out and thrown away downstream, which is the whole point: the data
            // pages of a skipped group are never read, never decompressed and never decoded. The row
            // counter still moves, because a later group's rows keep the numbers the file gives them.
            let metadata = reader.metadata();
            while cutting.group < cutting.groups {
                let Some(group) = metadata.row_groups.get(cutting.group) else { break };
                if !skips(&cutting.skipping, group, &metadata.schema) {
                    break;
                }
                cutting.group += 1;
                cutting.row = cutting.row.saturating_add(group.rows);
                cutting.skipped = cutting.skipped.saturating_add(1);
                if let Some(counters) = &self.counters {
                    counters.part_pruned();
                }
            }
        }
        let piece = match cutting.reader.as_ref() {
            Some(FileReader::Parquet(reader)) if cutting.group < cutting.groups => {
                let at = cutting.group;
                let rows = group_rows(reader, at);
                // A run of small groups goes out as one morsel. It stops at a group the filter
                // rules out, which the loop above steps over when the next morsel is cut.
                let taken = if cutting.part == 0 && cutting.gather > rows {
                    let metadata = reader.metadata();
                    let later = metadata.row_groups.get(at + 1..cutting.groups).unwrap_or(&[]);
                    let later = later
                        .iter()
                        .take_while(|group| !skips(&cutting.skipping, group, &metadata.schema))
                        .map(|group| usize::try_from(group.rows).unwrap_or(usize::MAX));
                    gathered(rows, later, cutting.gather)
                } else {
                    1
                };
                if taken > 1 {
                    let covered = (at..at + taken)
                        .map(|group| group_rows(reader, group))
                        .fold(0_usize, usize::saturating_add);
                    if let Some(counters) = &self.counters {
                        for _ in 0..taken {
                            counters.part_read();
                        }
                    }
                    let split = reader.split(at..at + taken)?;
                    cutting.group = at + taken;
                    let row = cutting.row;
                    cutting.row =
                        cutting.row.saturating_add(i64::try_from(covered).unwrap_or(i64::MAX));
                    return Ok(Some(Piece {
                        file,
                        reader: Some(FileReader::Parquet(split)),
                        failure: None,
                        row,
                    }));
                }
                let piece = next_piece(rows, cutting.part, cutting.cut);
                // Once per row group and not once per piece, so that the two counts on the
                // operator row are both in row groups and a reader can add them up.
                if piece.start == 0 {
                    if let Some(counters) = &self.counters {
                        counters.part_read();
                    }
                }
                let split = reader.split_rows(at, piece.clone())?;
                if piece.end >= rows {
                    cutting.group += 1;
                    cutting.part = 0;
                } else {
                    cutting.part = piece.end;
                }
                let row = cutting.row;
                cutting.row = cutting
                    .row
                    .saturating_add(i64::try_from(piece.end - piece.start).unwrap_or(i64::MAX));
                Piece { file, reader: Some(FileReader::Parquet(split)), failure: None, row }
            }
            Some(FileReader::Csv(_)) => {
                Piece { file, reader: cutting.reader.take(), failure: None, row: 0 }
            }
            // Either the row groups of the file being cut have all been handed out, or there is no
            // file being cut at all, and both mean the same thing to the caller.
            _ => return Ok(None),
        };
        Ok(Some(piece))
    }

    /// Registers a morsel's worth of work and hands back the morsel that covers it.
    fn hand(&self, cutting: &mut Cutting, piece: Piece) -> Option<Morsel> {
        let index = cutting.given;
        cutting.given += 1;
        let covers = u64::from(piece.failure.is_none());
        self.open.lock().ok()?.insert(index, Arc::new(Mutex::new(piece)));
        Some(Morsel::new(index, 0, covers))
    }

    /// What the morsel of this index covers.
    fn piece(&self, index: u64) -> Result<Arc<Mutex<Piece>>> {
        let open = self.open.lock().map_err(poisoned)?;
        open.get(&index)
            .map(Arc::clone)
            .ok_or_else(|| Error::internal(format!("a file scan was read at morsel {index}")))
    }

    /// The chunk with the row number column on the end of it.
    ///
    /// Built rather than read, because no file holds it. The values are a run, and the reason this
    /// is a loop over a range rather than a sequence vector is that the scan's consumer is free to
    /// slice or gather the chunk and a flat column survives both without a case.
    fn number(&self, chunk: Chunk, piece: &mut Piece) -> Result<Chunk> {
        let rows = chunk.len();
        let mut columns = Vec::with_capacity(chunk.width() + 1);
        for at in 0..chunk.width() {
            columns.push(chunk.column(at)?.clone());
        }
        let first = piece.row;
        piece.row = piece.row.saturating_add(i64::try_from(rows).unwrap_or(i64::MAX));
        let mut data = Vec::with_capacity(rows);
        for at in 0..rows {
            data.push(first.saturating_add(i64::try_from(at).unwrap_or(i64::MAX)));
        }
        columns.push(Vector::flat(LogicalType::BigInt, Data::Int64(data.into()))?);
        Chunk::with_rows(columns, rows)
    }

    /// The chunk with every column in the type the first file gave it.
    ///
    /// Almost always nothing, because almost always every file has the same schema, and the check is
    /// a type comparison per column per chunk rather than per row.
    ///
    /// `file` is which of the scan's paths this chunk came out of, and the only thing it is for is
    /// naming that file if a column will not cast.
    fn conform(&self, chunk: Chunk, file: usize) -> Result<Chunk> {
        let rows = chunk.len();
        let settled = self.wanted.iter().enumerate().all(|(at, field)| {
            chunk.column(at).is_ok_and(|column| column.logical_type() == &field.ty)
        });
        if settled {
            return Ok(chunk);
        }
        let mut columns = Vec::with_capacity(self.wanted.len());
        for (at, field) in self.wanted.iter().enumerate() {
            let column = chunk.column(at)?;
            if column.logical_type() == &field.ty {
                columns.push(column.clone());
                continue;
            }
            columns.push(cast(column, &field.ty, false).map_err(|error| {
                let path = self.paths.get(file).map_or("", String::as_str);
                Error::conversion(format!(
                    "Error while reading file \"{path}\": failed to cast column \"{}\" from type \
                     {} to {}: {}",
                    field.name,
                    column.logical_type(),
                    field.ty,
                    error.message()
                ))
            })?);
        }
        Chunk::with_rows(columns, rows)
    }
}

impl Source for FileScan<'_> {
    fn morsel(&self) -> Option<Morsel> {
        let mut cutting = self.cutting.lock().ok()?;
        loop {
            match self.cut(&mut cutting) {
                Ok(Some(piece)) => return self.hand(&mut cutting, piece),
                Ok(None) => {}
                Err(error) => {
                    let file = cutting.at.saturating_sub(1);
                    let piece = Piece { file, reader: None, failure: Some(error), row: 0 };
                    return self.hand(&mut cutting, piece);
                }
            }
            if cutting.at >= self.paths.len() {
                return None;
            }
            if let Err(error) = self.advance(&mut cutting) {
                let file = cutting.at.saturating_sub(1);
                let piece = Piece { file, reader: None, failure: Some(error), row: 0 };
                return self.hand(&mut cutting, piece);
            }
        }
    }

    /// The first file's morsels, taken as what every file in the list looks like.
    ///
    /// The first file is already open, because opening it is how a scan reports a file that has
    /// gone missing since it was bound, so what its row groups come to is there to be read without
    /// opening anything. The rest are assumed to match, which is right for a directory written by
    /// one writer and is the case worth being right about. A CSV file is cut into ranges here, now
    /// that the number of threads is known, so a list of CSV files is as many morsels as the first
    /// file has ranges for every file.
    fn morsels(&self, threads: usize, _weight: usize) -> Option<usize> {
        let mut cutting = self.cutting.lock().ok()?;
        cutting.threads = threads;
        aim(&mut cutting);
        self.divide(&mut cutting);
        Some(cutting.pieces.max(1).saturating_mul(self.paths.len().max(1)))
    }

    /// Remembered for [`aim`], which [`Source::morsels`] calls next.
    fn gather(&self, rows: usize) {
        if let Ok(mut cutting) = self.cutting.lock() {
            cutting.asked = rows;
        }
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let piece = self.piece(morsel.index())?;
        let mut piece = piece.lock().map_err(poisoned)?;
        if let Some(error) = piece.failure.take() {
            return Err(error);
        }
        loop {
            let file = piece.file;
            let Some(reader) = piece.reader.as_mut() else {
                morsel.advance(1);
                *out = Chunk::empty(&self.schema.types());
                return Ok(Progress::Done);
            };
            let before = reader.bytes_read();
            let next = reader.next_chunk()?;
            if let Some(counters) = &self.counters {
                counters.read(reader.bytes_read().saturating_sub(before));
            }
            let Some(chunk) = next else {
                // A file that is empty gives no chunk rather than an empty one, so this is not the
                // place that skips it. Taking the reader away and going round again is.
                piece.reader = None;
                continue;
            };
            let mut chunk = self.conform(chunk, file)?;
            if self.numbered {
                chunk = self.number(chunk, &mut piece)?;
            }
            // After the row numbers rather than before them, because the number a row carries is
            // its ordinal in the file and dropping rows first would renumber the ones that are
            // left.
            self.sift(&mut chunk)?;
            *out = chunk;
            return Ok(Progress::More);
        }
    }
}

/// One open file, whichever of the two readers it needed.
///
/// An enum rather than a trait because there are two of them and they are both in this workspace.
/// What is behind the two is not alike at all, which is the reason the enum is here rather than the
/// readers being made to look the same: a Parquet file states its schema and stores each column
/// apart, so reading two of a hundred and five is reading two stretches of the file, while a CSV
/// file states nothing and interleaves everything, so every byte is parsed whatever the projection
/// is and the projection only saves the conversion and the copy. The three calls they do share are
/// exactly the three the scan above needs.
///
/// A range of a CSV file cut up by [`FileScan::divide`] is a third kind, which only a morsel holds.
/// It was projected and settled as the whole file before it was cut, so it is only ever asked for
/// chunks.
#[derive(Debug)]
enum FileReader {
    Parquet(Reader),
    Csv(CsvReader),
    Part(Part),
}

impl FileReader {
    /// Opens `path` with the reader `function` names.
    fn open(function: TableFunction, path: &str, given: Given) -> Result<Self> {
        match function {
            TableFunction::ReadCsv => Ok(Self::Csv(open_csv(path, given)?)),
            _ => Ok(Self::Parquet(open_parquet(path)?)),
        }
    }

    /// The columns the file holds, in the order it holds them.
    fn fields(&self) -> Vec<Field> {
        match self {
            Self::Parquet(reader) => reader.fields(),
            Self::Csv(reader) => reader.fields(),
            Self::Part(_) => Vec::new(),
        }
    }

    /// Reads only these columns, by position in the file, in this order.
    fn project(&mut self, columns: &[usize]) -> Result<()> {
        match self {
            Self::Parquet(reader) => reader.project(columns),
            Self::Csv(reader) => reader.project(columns),
            Self::Part(_) => Err(cut_already()),
        }
    }

    /// Tells the file the types the whole read settled on, where that is a thing to say.
    ///
    /// It is one thing for Parquet and everything for CSV. A Parquet file states its types and the
    /// first file's are the read's, so a later file that disagrees is read as what it holds and cast
    /// by [`FileScan::conform`]. The exception is a byte array column the plan wants as text, which
    /// is `binary_as_string` arriving as the answer it produced rather than as a flag of its own,
    /// and which is a rename rather than a conversion. A CSV file has no types of its own, only the
    /// ones a sample of it suggested, and the read's came from combining the samples of every file,
    /// so this replaces the suggestion before a row is parsed rather than converting twice.
    fn settle(&mut self, wanted: &[Field]) -> Result<()> {
        match self {
            Self::Parquet(reader) => {
                let text: Vec<bool> =
                    wanted.iter().map(|field| field.ty == LogicalType::Varchar).collect();
                reader.as_string(&text);
                Ok(())
            }
            Self::Csv(reader) => {
                let types: Vec<LogicalType> = wanted.iter().map(|field| field.ty.clone()).collect();
                reader.retype(&types)
            }
            Self::Part(_) => Err(cut_already()),
        }
    }

    /// The next chunk, or `None` at the end of the file.
    fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        match self {
            Self::Parquet(reader) => reader.next_chunk(),
            Self::Csv(reader) => reader.next_chunk(),
            Self::Part(part) => part.next_chunk(),
        }
    }

    /// How many row groups the file has, which is how many morsels it is worth.
    ///
    /// Zero for CSV, which has none, and whose morsels are its ranges.
    fn row_groups(&self) -> usize {
        match self {
            Self::Parquet(reader) => reader.metadata().row_groups.len(),
            Self::Csv(_) | Self::Part(_) => 0,
        }
    }

    /// Bytes of the file read so far, which for Parquet is the compressed column bytes.
    fn bytes_read(&self) -> u64 {
        match self {
            Self::Parquet(reader) => reader.bytes_read(),
            Self::Csv(reader) => reader.bytes_read(),
            Self::Part(part) => part.bytes_read(),
        }
    }
}

/// What asking a range of a CSV file to be something other than read comes to, which is a scan
/// that cut a file before it finished opening it.
fn cut_already() -> Error {
    Error::internal("a range of a CSV file was reshaped after the file was cut")
}

/// What the call's named parameters said about how the CSV files are written.
///
/// The binder worked this out to sniff the files with and wrote the names and the values into the
/// plan, and this works it out again from them to read the files with. Both go through
/// [`csv_given`], so a file is read the way it was sniffed and the columns a query was planned
/// against are the columns it reads. A `read_parquet` call has none of these and gets the default,
/// which says nothing and is never asked.
fn csv_options(plan: &Plan, options: Slice, settings: Slice) -> Result<Given> {
    if options.len == 0 {
        return Ok(Given::default());
    }
    let exprs: Vec<ExprRef> = plan.expr_list(settings).to_vec();
    let source = Schema::empty();
    let one = Chunk::with_rows(Vec::new(), 1)?;
    let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
    let names: Vec<&str> = plan.name_list(options).iter().map(|name| plan.string(*name)).collect();
    let written: Vec<(&str, Value)> =
        names.into_iter().zip(evaluated.iter().map(|vector| vector.value_at(0))).collect();
    csv_given(&written)
}

/// The file names a file reading table function was called with.
///
/// The binder already refused anything that is not a constant string and already expanded whatever
/// patterns there were, so a failure here is a plan that was built wrong rather than a statement
/// somebody wrote wrong, and it says so.
pub(crate) fn file_arguments(
    plan: &Plan,
    args: Slice,
    function: TableFunction,
) -> Result<Vec<String>> {
    let exprs: Vec<ExprRef> = plan.expr_list(args).to_vec();
    let source = Schema::empty();
    let one = Chunk::with_rows(Vec::new(), 1)?;
    let evaluated = evaluate_all(plan, &exprs, &source, &one)?;
    let mut paths = Vec::with_capacity(evaluated.len());
    for vector in &evaluated {
        match vector.value_at(0) {
            Value::Varchar(path) => paths.push(path),
            other => {
                return Err(Error::internal(format!(
                    "{}() bound with {other:?} rather than constant file names",
                    function.name()
                )));
            }
        }
    }
    Ok(paths)
}

/// Where in `held` each of `wanted` is, by name.
///
/// `first` is the file the schema came from, and is `None` when `path` is that file. The two say
/// different things about a missing column and DuckDB writes both: the first file is the one the
/// plan was bound against, so a column missing from it means the file was replaced since, while a
/// column missing from a later one means the files in the set do not agree with each other.
///
/// The disagreement is worded by whichever reader found it, because the two readers in DuckDB are
/// two pieces of code that each wrote their own sentence and a compatibility test that compares
/// output compares all of it. For CSV this is only reachable when a file changed between binding and
/// running, since the binder sniffed every file and would have said the same thing first.
pub(crate) fn positions(
    function: TableFunction,
    wanted: &[Field],
    held: &[Field],
    path: &str,
    first: Option<&str>,
) -> Result<Vec<usize>> {
    let mut positions = Vec::with_capacity(wanted.len());
    for field in wanted {
        let at = held.iter().position(|column| column.name == field.name).ok_or_else(|| {
            let Some(first) = first else {
                return Error::io(format!(
                    "File \"{path}\" does not have a column named \"{}\"",
                    field.name
                ));
            };
            if matches!(function, TableFunction::ReadCsv) {
                return rudb_csv::mismatch(first, path, &field.name);
            }
            let candidates: Vec<&str> = held.iter().map(|column| column.name.as_str()).collect();
            Error::invalid_input(format!(
                "Failed to read file \"{path}\": schema mismatch in glob: column \"{}\" was read \
                 from the original file \"{first}\", but could not be found in file \
                 \"{path}\".\nCandidate names: {}\nIf you are trying to read files with different \
                 schemas, try setting union_by_name=True",
                field.name,
                candidates.join(", ")
            ))
        })?;
        positions.push(at);
    }
    Ok(positions)
}

impl Source for Values {
    fn morsel(&self) -> Option<Morsel> {
        self.handout.take()
    }

    fn morsels(&self, _threads: usize, _weight: usize) -> Option<usize> {
        Some(self.handout.total())
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        *out = match self.chunks.get(position(morsel)) {
            Some(chunk) => chunk.clone(),
            None => Chunk::empty(&self.schema.types()),
        };
        morsel.advance(1);
        Ok(Progress::Done)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rudb_catalog::{QualifiedName, Table};
    use rudb_common::{Field, LogicalType, Value};
    use rudb_functions::TableFunction;
    use rudb_metrics::Counters;
    use rudb_pipeline::{Progress, Source};
    use rudb_plan::{ColumnBinding, Node, Plan};
    use rudb_storage::Blocked;
    use rudb_vector::{Chunk, Data, Vector};

    use super::{
        Across, Bound, Cutoff, FileScan, Filters, Handout, Live, OnceLock, Op, Paying, Probe,
        Pushdown, RUN, Scan, Schema, Series, Session, Settings, Sideways, VECTOR_SIZE, WARMUP,
        cut_rows, gather_target, gathered, hash, instances_for, morsels_of, next_piece, parts,
        runs_of, worth_sifting,
    };
    use crate::sideways::Found;

    /// Every morsel a row group of `rows` rows is cut into, by asking for them the way `cut` does.
    fn cutting(rows: usize, target: usize) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut part = 0;
        loop {
            let piece = next_piece(rows, part, target);
            if piece.is_empty() {
                return out;
            }
            part = piece.end;
            out.push(piece);
            assert!(out.len() <= rows + 1, "cutting {rows} rows into {target} did not terminate");
        }
    }

    /// A series without going through a plan, which is what `Series::new` is for.
    fn series(start: i64, step: i64, rows: u64) -> Series {
        Series { start, step, rows, ..Series::empty(Schema::empty()) }
    }

    /// Every value the series produces, and how many morsels it took to produce them.
    fn drained(series: &Series) -> (Vec<i64>, usize) {
        let mut values = Vec::new();
        let mut morsels = 0;
        while let Some(mut morsel) = series.morsel() {
            morsels += 1;
            loop {
                let mut chunk = Chunk::empty(&[LogicalType::BigInt]);
                let progress = series.read(&mut morsel, &mut chunk).expect("a series reads");
                for row in 0..chunk.len() {
                    match chunk.value_at(row, 0) {
                        Value::BigInt(value) => values.push(value),
                        other => panic!("a series produced {other}"),
                    }
                }
                if progress == Progress::Done {
                    break;
                }
            }
        }
        (values, morsels)
    }

    /// A morsel holds more than a chunk, so reading one is several calls, and the last of them is
    /// the one that says the morsel is done.
    #[test]
    fn a_morsel_of_a_series_is_read_a_chunk_at_a_time() {
        let rows = VECTOR_SIZE as u64 * 2 + 5;
        let (values, morsels) = drained(&series(0, 1, rows));

        assert_eq!(morsels, 1);
        assert_eq!(values.len(), rows as usize);
        assert_eq!(values[0], 0);
        assert_eq!(values[values.len() - 1], rows as i64 - 1);
    }

    /// The value at a position is worked out from the position rather than counted up to, because
    /// the thread that gets the second morsel never saw the first one.
    #[test]
    fn a_series_longer_than_a_morsel_carries_on_where_the_last_one_stopped() {
        let rows = RUN + 3;
        let (values, morsels) = drained(&series(10, 3, rows));

        assert_eq!(morsels, 2);
        assert_eq!(values.len(), rows as usize);
        assert_eq!(values[0], 10);
        assert_eq!(values[RUN as usize], 10 + 3 * RUN as i64);
        assert_eq!(values[values.len() - 1], 10 + 3 * (rows as i64 - 1));
    }

    /// Nothing to produce is no morsels at all, rather than one morsel that produces nothing, which
    /// is what a null argument to `range()` means.
    #[test]
    fn a_series_of_nothing_hands_out_no_work() {
        let (values, morsels) = drained(&series(0, 1, 0));

        assert!(values.is_empty());
        assert_eq!(morsels, 0);
    }

    /// The whole of what makes a source shareable: a position goes to one caller and the next
    /// caller gets the next one, so two threads scanning a table read different chunks of it.
    #[test]
    fn a_handout_gives_each_position_to_one_caller_and_then_stops() {
        let handout = Handout::new(3);

        let taken: Vec<u64> = (0..3).map(|_| handout.take().expect("a position").start()).collect();

        assert_eq!(taken, [0, 1, 2]);
        assert!(handout.take().is_none());
        assert!(handout.take().is_none());
    }

    /// A native file of `parts` parts of `rows` rows each, and the catalog table over it.
    ///
    /// The file is written rather than faked because the thing under test is what the scan does
    /// with the stripes the format puts the parts in, and only the format knows where those are.
    fn native(label: &str, parts: usize, rows: usize) -> (Table, std::path::PathBuf) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time advances")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("rudb-exec-{label}-{}-{stamp}.rdb", std::process::id()));
        let mut writer = rudb_native::Writer::create(
            &path,
            "items",
            vec![Field::required("id", LogicalType::Integer)],
        )
        .expect("a new file");
        for part in 0..parts {
            let values: Vec<Value> =
                (0..rows).map(|row| Value::Integer((part * rows + row) as i32)).collect();
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::Integer, &values).expect("integers"),
            ])
            .expect("matching rows");
            writer.append(&chunk).expect("one part");
        }
        writer.finish().expect("commit");
        let reader = rudb_native::Reader::open(&path).expect("reopen from disk");
        let table = Table::native(QualifiedName::new("memory", "main", "items"), reader)
            .expect("a native table");
        (table, path)
    }

    /// A scan of a table, built the way the builder builds one.
    fn scan_of(table: &Table) -> Scan<'_> {
        let plan = Plan::parse("Get memory.main.items AS items #0 [id::INTEGER]")
            .expect("the plan text round trips");
        let Node::Get { index, columns, .. } = *plan.node(plan.root()) else {
            panic!("the plan is a get");
        };
        Scan::new(
            &plan,
            table,
            index,
            columns,
            Filters::default(),
            &Settings::default(),
            &Session::default(),
        )
        .expect("the column is there")
    }

    /// Every morsel the scan hands out, as the parts it covers and the rows that came out of it.
    fn taken(scan: &Scan<'_>) -> Vec<(std::ops::Range<u64>, usize)> {
        let mut out = Vec::new();
        while let Some(mut morsel) = scan.morsel() {
            let mut rows = 0;
            loop {
                let mut chunk = Chunk::empty(&[]);
                let progress = scan.read(&mut morsel, &mut chunk).expect("a part reads");
                rows += chunk.len();
                if progress == Progress::Done {
                    break;
                }
            }
            out.push((morsel.start()..morsel.end(), rows));
        }
        out
    }

    /// A whole stripe per morsel, once there are at least as many stripes as workers.
    ///
    /// This is what stops the workers racing for a page. Handing out parts puts every worker in the
    /// same stripe at once, one of them reads the page and the rest read their own part out of the
    /// same bytes, so a quarter of what a cold column moves is moved twice and the rest moves in
    /// reads too small to keep the disk busy. A morsel that is a whole stripe gives the page to one
    /// worker and there is nothing left to race for.
    #[test]
    fn a_native_scan_with_a_stripe_for_every_worker_hands_out_stripes() {
        let (table, path) = native("stripes", 128, 200);
        let scan = scan_of(&table);
        let stripes = table.rows().stripe_parts();
        assert_eq!(stripes.len(), 2, "two full stripes of sixty four parts");

        assert_eq!(scan.morsels(2, 1), Some(2), "one instance per stripe");
        let taken = taken(&scan);

        assert_eq!(taken.len(), 2, "one morsel per stripe");
        assert_eq!(taken[0].0, 0..64);
        assert_eq!(taken[1].0, 64..128);
        assert_eq!(taken.iter().map(|(_, rows)| rows).sum::<usize>(), 128 * 200);
        std::fs::remove_file(path).expect("remove scratch file");
    }

    /// A table with fewer stripes than workers keeps handing out parts.
    ///
    /// A stripe per worker would leave every worker but one with nothing at all, and the duplicate
    /// reads it saves are worth less than the rest of the machine.
    #[test]
    fn a_native_scan_with_one_stripe_hands_out_parts() {
        let (table, path) = native("one-stripe", 64, 400);
        let scan = scan_of(&table);
        assert_eq!(table.rows().stripe_parts().len(), 1, "one stripe and more workers than that");

        assert_eq!(scan.morsels(4, 1), Some(2), "the rows behind it pay for two instances");
        let taken = taken(&scan);

        assert_eq!(taken.len(), 64, "one morsel per part");
        assert_eq!(taken[0].0, 0..1);
        assert_eq!(taken.iter().map(|(_, rows)| rows).sum::<usize>(), 64 * 400);
        std::fs::remove_file(path).expect("remove scratch file");
    }

    /// A scan of a native table with bounds tests on its only column, the way a filter compiles one.
    fn pruned_scan(table: &Table, tests: Vec<(usize, Op, Bound)>) -> Scan<'_> {
        let plan = Plan::parse("Get memory.main.items AS items #0 [id::INTEGER]")
            .expect("the plan text round trips");
        let Node::Get { index, columns, .. } = *plan.node(plan.root()) else {
            panic!("the plan is a get");
        };
        Scan::new(
            &plan,
            table,
            index,
            columns,
            Filters { pruning: tests, ..Filters::default() },
            &Settings::default(),
            &Session::default(),
        )
        .expect("the column is there")
    }

    /// A predicate that leaves one stripe of two still divides the work between two workers.
    ///
    /// Two things are being checked and the second is the one that matters. The instance count comes
    /// from the rows the zone maps leave rather than from the rows the table holds, so a scan that
    /// will read half a file asks for what half a file pays for. And the stripe that has work in it
    /// is cut up, because a morsel per stripe would have given one worker everything and the other
    /// nothing, which is how ClickBench 39 came to be faster on one thread than on sixteen.
    #[test]
    fn a_native_scan_divides_the_stripe_that_survives_pruning() {
        let (table, path) = native("pruned", 128, 400);
        assert_eq!(table.rows().stripe_parts().len(), 2, "two full stripes of sixty four parts");
        let scan = pruned_scan(&table, vec![(0, Op::GreaterOrEqual, Bound::Int(25_600))]);

        assert_eq!(scan.morsels(2, 1), Some(2), "the surviving half pays for two instances");
        let taken = taken(&scan);

        assert!(taken.len() > 2, "the one stripe with work is cut up, not handed out whole");
        assert!(
            taken.iter().all(|(run, _)| run.start >= 64),
            "no morsel covers the ruled out stripe"
        );
        assert_eq!(taken.iter().map(|(_, rows)| rows).sum::<usize>(), 64 * 400);
        std::fs::remove_file(path).expect("remove scratch file");
    }

    /// The rule that decides whether to read the sieves before starting anybody.
    ///
    /// The case in the middle is ClickBench 19, an equality on an identifier over the whole file. No
    /// stripe bound rules anything out there, so all sixteen stripes come out of the first pass with
    /// work in them and there is nothing left for a better instance count to fix. Reading the sieves
    /// anyway would find that only nine parts matter, on one thread, while sixteen workers that were
    /// about to find the same thing at the same time waited for it, and that cost the query a third
    /// of its time.
    #[test]
    fn the_sieves_are_read_early_only_when_the_bounds_have_left_the_work_in_a_heap() {
        assert!(worth_sifting(2, 32, 1_000_000, 1), "two stripes of work and a machine to fill");
        assert!(
            !worth_sifting(16, 32, 1_000_000, 1),
            "sixteen is as many instances as the rows buy"
        );
        assert!(!worth_sifting(1, 1, 1_000_000, 1), "one worker has nowhere to spread it anyway");
        assert!(!worth_sifting(1, 32, 1_000, 1), "a thousand rows pay for one worker either way");
        assert!(worth_sifting(1, 32, 1_000, 64), "a thousand rows of heavy work pay for more");
    }

    /// The live parts of each stripe, with the rows they hold worked out the way `bounded` does.
    fn living(stripes: &[&[usize]], rows: impl Fn(usize) -> usize) -> Vec<Live> {
        stripes
            .iter()
            .map(|parts| Live {
                parts: parts.to_vec(),
                rows: parts.iter().map(|&at| rows(at)).sum(),
            })
            .collect()
    }

    /// The cutting rule on its own. A stripe holding no more than a share stays one morsel while
    /// there are as many stripes with work in them as there are workers, and once there are fewer the
    /// stripes are shared anyway and the cut gets finer.
    #[test]
    fn a_stripe_holding_no_more_than_a_share_stays_one_run() {
        let live = living(&[&[0, 1, 2, 3], &[4, 5, 6, 7], &[8, 9, 10, 11]], |_| 1);

        assert_eq!(runs_of(&live, 1, |_| 1), [0..4, 4..8, 8..12], "one worker, one run a stripe");
        assert_eq!(runs_of(&live, 3, |_| 1), [0..4, 4..8, 8..12], "a stripe is a share exactly");
        let four = runs_of(&live, 4, |_| 1);
        assert_eq!(four.len(), 12, "three stripes for four workers, so a part apiece");
        assert_eq!(four.first(), Some(&(0..1)));
        assert_eq!(four.last(), Some(&(11..12)));
    }

    /// The reason the cut is by rows. Three stripes with work in them are not three pieces of work
    /// when the statistics left one of them holding almost all of it, and the old rule handed that
    /// one to a single worker while the other two finished at once.
    #[test]
    fn the_stripe_holding_the_rows_is_the_one_that_gets_cut() {
        let rows = |at: usize| if (1..9).contains(&at) { 1_000 } else { 10 };
        let live = living(&[&[0], &[1, 2, 3, 4, 5, 6, 7, 8], &[9]], rows);

        let runs = runs_of(&live, 2, rows);

        assert_eq!(runs, [0..1, 1..3, 3..5, 5..7, 7..9, 9..10], "the big stripe in four, not one");
        let held: Vec<usize> = runs.iter().map(|run| run.clone().map(rows).sum()).collect();
        assert_eq!(held, [10, 2_000, 2_000, 2_000, 2_000, 10], "and the four are the same size");
    }

    /// And the other half of it. One stripe holding all the work is cut into runs of its live parts,
    /// and the parts the zone maps ruled out are not in any of them except where they sit between
    /// two that survived, which a morsel walks past without reading.
    #[test]
    fn a_stripe_that_holds_all_the_work_is_cut_up() {
        let live = living(&[&[1, 3, 5, 7], &[]], |_| 1);

        let runs = runs_of(&live, 2, |_| 1);

        assert_eq!(runs, [1..2, 3..4, 5..6, 7..8], "a run apiece, two workers, four to go round");
        assert!(runs.iter().all(|run| run.start >= 1 && run.end <= 8));
    }

    /// A scan of the `rudb-parquet` fixture, which is 4096 rows in two row groups.
    ///
    /// Built from a plan written as text, the way every other operator test in this crate builds
    /// one, because the fields a table function node carries are arena slices and writing them out
    /// by hand would be a test of the arena builders.
    fn fixture() -> FileScan<'static> {
        pruned(Vec::new())
    }

    /// The same scan with bounds tests on it, which is what a filter above the scan compiles to.
    ///
    /// Told it has two threads, as the scheduler tells every scan before it takes a morsel, so the
    /// two groups are a share each and are handed out apart rather than gathered.
    fn pruned(tests: Vec<(usize, Op, Bound)>) -> FileScan<'static> {
        let scan = untold(tests);
        assert_eq!(scan.morsels(2, 0), Some(2), "a group for each of the two threads");
        scan
    }

    /// The scan as it is before the scheduler says anything, which is one thread.
    fn untold(tests: Vec<(usize, Op, Bound)>) -> FileScan<'static> {
        let path = format!("{}/../rudb-parquet/testdata/mixed.parquet", env!("CARGO_MANIFEST_DIR"));
        let path = path.replace('\\', "\\\\").replace('\'', "''");
        let text = format!(
            "TableFunction read_parquet args=['{path}'::VARCHAR] #0 [a::INTEGER, b::BIGINT]"
        );
        let plan = Plan::parse(&text).expect("the plan text round trips");
        let Node::TableFunction { index, args, options, settings, columns, .. } =
            *plan.node(plan.root())
        else {
            panic!("the plan is a table function");
        };
        FileScan::new(
            &plan,
            index,
            TableFunction::ReadParquet,
            args,
            options,
            settings,
            columns,
            tests,
            None,
        )
        .expect("the fixture is there")
    }

    /// Every morsel the scan hands out, drained.
    fn morsels(scan: &FileScan<'_>) -> Vec<usize> {
        let mut rows = Vec::new();
        while let Some(mut morsel) = scan.morsel() {
            let mut chunk = Chunk::empty(&[]);
            let mut read = 0;
            while let Progress::More = scan.read(&mut morsel, &mut chunk).expect("decodes") {
                read += chunk.len();
            }
            rows.push(read);
        }
        rows
    }

    /// The property the scheduler needs. A file of two row groups is two units of work, not one,
    /// and the two between them hold every row of the file.
    #[test]
    fn a_parquet_file_is_one_morsel_per_row_group() {
        let scan = fixture();
        let rows = morsels(&scan);
        assert_eq!(rows, [2048, 2048], "two row groups of 2048");
    }

    /// The point of the bounds tests. The integer column of the fixture runs 0 to 96, so a filter
    /// asking for rows above a thousand cannot be satisfied by either row group, and neither group
    /// is handed out at all. No morsel means no page was read, which is the whole saving.
    #[test]
    fn a_row_group_whose_bounds_rule_out_the_filter_is_never_handed_out() {
        let scan = pruned(vec![(0, Op::Greater, Bound::Int(1_000))]);

        let rows = morsels(&scan);

        assert_eq!(rows, Vec::<usize>::new(), "both row groups are ruled out");
        let cutting = scan.cutting.lock().expect("the lock holds");
        assert_eq!(cutting.skipped, 2, "and both were skipped rather than read");
    }

    /// The other half of it. A bound that overlaps the column leaves the scan exactly as it was,
    /// because a row group the statistics cannot rule out has to be read and filtered as usual.
    #[test]
    fn a_row_group_whose_bounds_overlap_the_filter_is_handed_out_as_usual() {
        let scan = pruned(vec![(0, Op::Greater, Bound::Int(50))]);

        let rows = morsels(&scan);

        assert_eq!(rows, [2048, 2048], "nothing is ruled out");
        let cutting = scan.cutting.lock().expect("the lock holds");
        assert_eq!(cutting.skipped, 0);
    }

    /// A test naming a column the scan does not produce is dropped rather than misread as column
    /// zero, which would skip row groups holding rows the query wants.
    #[test]
    fn a_test_against_a_position_the_scan_does_not_produce_rules_nothing_out() {
        let scan = pruned(vec![(7, Op::Greater, Bound::Int(1_000))]);

        assert_eq!(morsels(&scan), [2048, 2048]);
    }

    /// Morsels are taken until they run out, and asking after that hands back nothing rather than
    /// starting again, which is what makes a driver loop safe to write as a `while let`.
    #[test]
    fn a_scan_that_has_handed_out_every_morsel_hands_out_no_more() {
        let scan = fixture();
        let _ = morsels(&scan);
        assert!(scan.morsel().is_none());
        assert!(scan.morsel().is_none());
    }

    /// Each morsel carries its own reader, so taking them all before reading any of them reads the
    /// same rows as taking and reading them one at a time. That is the difference between a scan
    /// two threads can share and a scan they queue behind.
    #[test]
    fn every_morsel_can_be_taken_before_any_of_them_is_read() {
        let scan = fixture();
        let mut taken = Vec::new();
        while let Some(morsel) = scan.morsel() {
            taken.push(morsel);
        }
        assert_eq!(taken.len(), 2);
        let mut rows = Vec::new();
        for morsel in &mut taken {
            let mut chunk = Chunk::empty(&[]);
            let mut read = 0;
            while let Progress::More = scan.read(morsel, &mut chunk).expect("decodes") {
                read += chunk.len();
            }
            rows.push(read);
        }
        assert_eq!(rows, [2048, 2048]);
    }

    /// A table of one `INTEGER` column holding `0..rows`, which puts a different range in every
    /// chunk and so makes the zone maps worth having.
    fn counted(rows: usize) -> Table {
        let mut table = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![Field::new("n", LogicalType::Integer)],
        )
        .expect("one column");
        let values: Vec<Vec<Value>> = (0..rows).map(|n| vec![Value::Integer(n as i32)]).collect();
        table.append_rows(&values).expect("integers");
        table
    }

    /// A scan of that table with `tests` on its only column, built without going through a plan.
    fn scanning(table: &Table, tests: Vec<(usize, Op, Bound)>) -> Scan<'_> {
        let fields = vec![Field::new("n", LogicalType::Integer)];
        let probes =
            tests.into_iter().map(|(column, op, value)| Probe { column, op, value }).collect();
        Scan {
            table,
            columns: vec![Some(0)],
            offsets: (0..table.rows().chunk_count())
                .map(|at| i64::try_from(at * VECTOR_SIZE).expect("a small table"))
                .collect(),
            probes,
            also: Vec::new(),
            sideways: None,
            cutoff: None,
            index: 0,
            testing: OnceLock::new(),
            schema: Schema::numbered(fields, 0),
            chunks: Handout::new(table.rows().chunk_count()),
            stripes: Vec::new(),
            pushed: None,
            waved: AtomicUsize::new(0),
            spread: OnceLock::new(),
            skipped: AtomicUsize::new(0),
            counters: None,
            deferred: Vec::new(),
            deferring: Paying::default(),
            paying: Paying::default(),
            passed: Paying::default(),
        }
    }

    /// A scan of that table under a top N that has filled its candidates and reached `bound`.
    ///
    /// No tests of its own, so the only thing that can rule a part out here is the cutoff.
    fn beaten(table: &Table, op: Op, bound: Option<Bound>) -> Scan<'_> {
        let cutoff = Cutoff::new();
        cutoff.about(ColumnBinding::new(0, 0), op);
        if let Some(bound) = bound {
            cutoff.reached(bound);
        }
        let mut scan = scanning(table, Vec::new());
        scan.cutoff = Some(cutoff);
        scan
    }

    /// A scan of `counted` applying `predicate` itself, the way the builder hands one over.
    ///
    /// The plan is written out and parsed rather than assembled, so the predicate this applies is
    /// the one a query would really have produced, casts and all.
    fn applying<'a>(table: &'a Table, predicate: &str) -> (Plan, Scan<'a>) {
        let plan =
            Plan::parse(&format!("Filter {predicate}\n  Get memory.main.t AS t #0 [n::INTEGER]"))
                .expect("the plan text round trips");
        let Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the plan is a filter");
        };
        let Node::Get { index, columns, .. } = *plan.node(input) else {
            panic!("under a get");
        };
        let moved =
            rudb_opt::bounds::into_scan(&plan, plan.root()).expect("a filter over a stored table");
        let pruning = rudb_opt::bounds::of(&plan, input, predicate);
        let pushdown =
            Pushdown { node: plan.root(), predicate, tests: moved.tests, whole: moved.whole };
        let filters = Filters { pruning, pushed: Some(pushdown), ..Filters::default() };
        let scan = Scan::new(
            &plan,
            table,
            index,
            columns,
            filters,
            &Settings::default(),
            &Session::default(),
        )
        .expect("the column is there");
        (plan, scan)
    }

    /// A necessary LIKE can read one column first without changing the full AND answer.
    #[test]
    fn a_sparse_like_fetches_the_second_column_after_selection() {
        let mut table = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![
                Field::new("URL", LogicalType::Varchar),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns");
        let rows = (0..VECTOR_SIZE * 3)
            .map(|row| {
                let matching =
                    row == 3 || row == 5 || (VECTOR_SIZE..VECTOR_SIZE + 400).contains(&row);
                let phrase = if row == 5 || row == VECTOR_SIZE + 10 { "" } else { "phrase" };
                vec![
                    Value::Varchar(if matching { "google.test" } else { "example.test" }.into()),
                    Value::Varchar(phrase.into()),
                ]
            })
            .collect::<Vec<_>>();
        table.append_rows(&rows).expect("rows");
        let plan = Plan::parse(
            "Filter (\"~~\"(#0.0::VARCHAR, '%google%'::VARCHAR)::BOOLEAN AND (#0.1::VARCHAR <> ''::VARCHAR)::BOOLEAN)::BOOLEAN\n  Get memory.main.t AS t #0 [URL::VARCHAR, SearchPhrase::VARCHAR]",
        )
        .expect("the plan text round trips");
        let Node::Filter { input, predicate } = *plan.node(plan.root()) else {
            panic!("the plan is a filter");
        };
        let Node::Get { index, columns, .. } = *plan.node(input) else { panic!("under a get") };
        let moved =
            rudb_opt::bounds::into_scan(&plan, plan.root()).expect("a filter over a stored table");
        let pushdown =
            Pushdown { node: plan.root(), predicate, tests: moved.tests, whole: moved.whole };
        let filters = Filters { pushed: Some(pushdown), ..Filters::default() };
        let scan = Scan::new(
            &plan,
            &table,
            index,
            columns,
            filters,
            &Settings::default(),
            &Session::default(),
        )
        .expect("two projected columns");
        assert!(scan.pushed.as_ref().and_then(|pushed| pushed.late.as_ref()).is_some());
        let mut found = Vec::new();
        while let Some(mut morsel) = scan.morsel() {
            loop {
                let mut chunk = Chunk::empty(&[]);
                let progress = scan.read(&mut morsel, &mut chunk).expect("a part reads");
                found.extend(
                    (0..chunk.len()).map(|row| (chunk.value_at(row, 0), chunk.value_at(row, 1))),
                );
                if progress == Progress::Done {
                    break;
                }
            }
        }
        let expected = rows
            .iter()
            .filter(|row| {
                matches!(&row[0], Value::Varchar(url) if url.contains("google"))
                    && matches!(&row[1], Value::Varchar(phrase) if !phrase.is_empty())
            })
            .map(|row| (row[0].clone(), row[1].clone()))
            .collect::<Vec<_>>();
        assert_eq!(found, expected, "sparse, dense, and empty parts keep the same rows");
    }

    /// The middle of the three answers. Every row of the table is at or above zero, so every chunk
    /// is one the zone waves through and the comparison never runs on any of them.
    #[test]
    fn a_filter_the_zone_maps_prove_is_applied_to_no_rows_at_all() {
        let table = counted(VECTOR_SIZE * 5);
        let (_plan, scan) = applying(&table, "(#0.0::INTEGER >= 0::INTEGER)::BOOLEAN");

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 5, "every row came through");
        assert_eq!(scan.waved.load(Ordering::Relaxed), 5, "and no chunk was compared");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0, "nor ruled out");
    }

    /// All three answers in one scan. Five chunks hold the rows a vector at a time and the filter
    /// keeps everything from halfway through the third one up, so the first two are ruled out, the
    /// third straddles the constant and is compared, and the last two are waved through whole.
    ///
    /// The cutoff is worked out from the vector size rather than written down, which it was until
    /// the vector stopped being 1024 and the constant landed inside the first chunk instead of the
    /// third. Every boundary in these tests is a multiple of the thing under test.
    #[test]
    fn a_scan_skips_waves_through_and_compares_in_the_one_pass() {
        let table = counted(VECTOR_SIZE * 5);
        let cutoff = VECTOR_SIZE * 2 + VECTOR_SIZE / 2;
        let (_plan, scan) =
            applying(&table, &format!("(#0.0::INTEGER >= {cutoff}::INTEGER)::BOOLEAN"));

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 5 - cutoff);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 2, "the first two hold nothing wanted");
        assert_eq!(scan.waved.load(Ordering::Relaxed), 2, "the last two are all of them wanted");
    }

    /// Every part of the table is counted exactly once, as read or as ruled out.
    ///
    /// The invariant matters because the two numbers go on the operator row as "n of m parts
    /// skipped", and an m that is not the table is a sentence that reads as true and is not. There
    /// are two places a part can be ruled out, the walk in [`Source::read`] and the runs
    /// [`Source::morsels`] hands out, and the easy mistake is to count one of them.
    #[test]
    fn the_two_part_counts_add_up_to_the_table() {
        let table = counted(VECTOR_SIZE * 5);
        let cutoff = VECTOR_SIZE * 2 + VECTOR_SIZE / 2;
        let (_plan, mut scan) =
            applying(&table, &format!("(#0.0::INTEGER >= {cutoff}::INTEGER)::BOOLEAN"));
        let counters = Arc::new(Counters::new(0, 0, "Scan"));
        scan = scan.watched(Arc::clone(&counters));

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 5 - cutoff);
        let operator = counters.snapshot();
        assert_eq!(operator.parts_pruned, 2, "the first two hold nothing wanted");
        assert_eq!(
            operator.parts_read + operator.parts_pruned,
            table.rows().chunk_count() as u64,
            "every part is counted once and only once"
        );
    }

    /// The filter is really applied and not only decided about.
    #[test]
    fn a_chunk_the_zone_cannot_decide_comes_back_narrowed_to_the_rows_that_pass() {
        let table = counted(VECTOR_SIZE);
        let (_plan, scan) = applying(&table, "(#0.0::INTEGER < 10::INTEGER)::BOOLEAN");

        assert_eq!(counted_rows(&scan), 10);
        assert_eq!(scan.waved.load(Ordering::Relaxed), 0);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// A predicate the zone maps cannot read still moves into the scan, and still answers.
    ///
    /// The `OR` is what ClickBench 40 writes as `TraficSourceID IN (-1, 6)`, and it reads as no test
    /// at all, because a conjunct under an `OR` says nothing about the row when it is false. That
    /// used to keep the whole filter above the scan. It does not any more: the comparison runs here,
    /// on every chunk, which is exactly what it would have done up there, and one operator and the
    /// chunk handed across to it are gone.
    ///
    /// Both counters staying at zero is the part worth pinning. Nothing may be waved through, since
    /// there is no test that proves anything about a row, and an empty test list read as a proof is
    /// the one way this arrangement returns rows the query asked to be rid of.
    #[test]
    fn a_filter_with_an_or_in_it_moves_into_the_scan_and_no_chunk_is_waved_through() {
        let table = counted(VECTOR_SIZE * 5);
        let high = VECTOR_SIZE * 5 - 10;
        let (_plan, scan) = applying(
            &table,
            &format!(
                "((#0.0::INTEGER < 10::INTEGER)::BOOLEAN \
                 OR (#0.0::INTEGER >= {high}::INTEGER)::BOOLEAN)::BOOLEAN"
            ),
        );

        assert_eq!(counted_rows(&scan), 20, "the ten at each end");
        assert_eq!(scan.waved.load(Ordering::Relaxed), 0, "and nothing was proved about a chunk");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// How many rows the scan produced, over every morsel it hands out.
    fn counted_rows(scan: &Scan<'_>) -> usize {
        let mut rows = 0;
        while let Some(mut morsel) = scan.morsel() {
            let mut chunk = Chunk::empty(&[LogicalType::Integer]);
            loop {
                let progress = scan.read(&mut morsel, &mut chunk).expect("a table scan reads");
                rows += chunk.len();
                if progress == Progress::Done {
                    break;
                }
            }
        }
        rows
    }

    /// The point of the zone maps. Five chunks hold 0 to 10239, and `n = 5000` is in exactly one of
    /// them, so four are never read at all and the filter above never sees their rows.
    #[test]
    fn a_filter_on_a_table_reads_only_the_chunks_that_can_hold_a_match() {
        let table = counted(VECTOR_SIZE * 5);
        assert_eq!(table.rows().chunk_count(), 5);
        let scan = scanning(&table, vec![(0, Op::Equal, Bound::Int(5_000))]);

        assert_eq!(counted_rows(&scan), VECTOR_SIZE, "one chunk's worth");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// The point of the cutoff. Five chunks hold 0 to 5119 and the top N above already holds ten
    /// rows whose worst key is 100, so every chunk but the first holds nothing that can still win.
    #[test]
    fn a_scan_skips_the_parts_the_top_n_above_it_has_already_beaten() {
        let table = counted(VECTOR_SIZE * 5);
        let scan = beaten(&table, Op::LessOrEqual, Some(Bound::Int(100)));

        assert_eq!(counted_rows(&scan), VECTOR_SIZE, "only the chunk holding 0 to 1023");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// The same read the other way up, which is the other comparison the ordering can give.
    #[test]
    fn a_descending_top_n_skips_the_parts_below_its_cutoff() {
        let table = counted(VECTOR_SIZE * 5);
        let cutoff = i128::try_from(VECTOR_SIZE * 4 + VECTOR_SIZE / 2).expect("a small number");
        let scan = beaten(&table, Op::GreaterOrEqual, Some(Bound::Int(cutoff)));

        assert_eq!(counted_rows(&scan), VECTOR_SIZE, "only the last chunk");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// A part that ties the cutoff is read like any other, which is what keeps ties settled the way
    /// they were. The third chunk starts exactly at the cutoff and is read.
    #[test]
    fn a_part_that_only_ties_the_cutoff_is_still_read() {
        let table = counted(VECTOR_SIZE * 5);
        let cutoff = i128::try_from(VECTOR_SIZE * 2).expect("a small number");
        let scan = beaten(&table, Op::LessOrEqual, Some(Bound::Int(cutoff)));

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 3);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 2);
    }

    /// Before the top N has filled its candidates there is nothing it can rule out, which is the
    /// first chunks of every query and the whole of a query whose limit is never reached.
    #[test]
    fn a_top_n_that_has_not_filled_its_candidates_rules_nothing_out() {
        let table = counted(VECTOR_SIZE * 3);
        let scan = beaten(&table, Op::LessOrEqual, None);

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 3);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// A scan with nothing to go on reads everything, which is the case that must not regress.
    #[test]
    fn a_scan_with_no_tests_reads_every_chunk() {
        let table = counted(VECTOR_SIZE * 3);
        let scan = scanning(&table, Vec::new());

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 3);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// The same skipping, from a range no plan could have carried: what a join above this scan found
    /// on its other side. Five chunks hold 0 to 10239 and the build side held keys 5000 to 5100, so
    /// only the chunk those fall in is read.
    #[test]
    fn a_scan_skips_the_chunks_a_joins_build_side_ruled_out() {
        let table = counted(VECTOR_SIZE * 5);
        let sideways = Sideways::new();
        sideways.about(ColumnBinding::new(0, 0));
        sideways.found(Found::of(Some((Bound::Int(5_000), Bound::Int(5_100))), None));
        let mut scan = scanning(&table, Vec::new());
        scan.sideways = Some(sideways);

        assert_eq!(counted_rows(&scan), VECTOR_SIZE, "one chunk's worth");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// The exact tier, which answers by position rather than by value. Five chunks, and the join
    /// above handed down ten rows of the third and one of the fifth, so three chunks are never read
    /// and only those eleven rows come out of the two that are.
    #[test]
    fn a_scan_keeps_exactly_the_rows_a_reduction_handed_down() {
        let table = counted(VECTOR_SIZE * 5);
        let size = u64::try_from(VECTOR_SIZE).expect("a small number");
        let kept: Vec<u64> = (2 * size + 5..2 * size + 15).chain([4 * size + 7]).collect();
        let sideways = Sideways::new();
        sideways.about(ColumnBinding::new(0, 0));
        let rows = rudb_graph::Rids::from_sorted(5 * size, kept).expect("in order");
        sideways.found(Found::exactly(None, rows));
        let mut scan = scanning(&table, Vec::new());
        scan.sideways = Some(sideways);

        assert_eq!(counted_rows(&scan), 11);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 3);
    }

    /// A filter over three keys, hashed the way the build side of a join hashes the column it is
    /// keyed on.
    fn holding(values: &[i32]) -> Blocked {
        let mut filter = Blocked::sized(values.len(), 1 << 20).expect("a filter over three keys");
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(&column), values.len(), &mut hashes, Across::TwoInputs);
        for word in hashes {
            filter.add(word);
        }
        filter
    }

    /// The row tier. The range keeps the chunk the three keys fall in and the filter throws away
    /// every row of it that is not one of them.
    #[test]
    fn a_scan_drops_the_rows_a_joins_build_side_cannot_match() {
        let table = counted(VECTOR_SIZE * 3);
        let sideways = Sideways::new();
        sideways.about(ColumnBinding::new(0, 0));
        sideways.found(Found::of(
            Some((Bound::Int(2_100), Bound::Int(2_300))),
            Some(holding(&[2_100, 2_200, 2_300])),
        ));
        let mut scan = scanning(&table, Vec::new());
        scan.sideways = Some(sideways);

        assert_eq!(counted_rows(&scan), 3, "the three rows the build side holds a key for");
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 2, "and two chunks were never read");
    }

    /// A filter that keeps nearly every row it sees is dropped once the warmup is over, and one
    /// that turns rows away is kept however long the scan runs.
    #[test]
    fn a_runtime_filter_that_turns_nothing_away_is_given_up_on() {
        let idle = Paying::default();
        let busy = Paying::default();
        for _ in 0..(WARMUP / VECTOR_SIZE) {
            assert!(idle.worth(), "nothing is decided under the warmup");
            idle.saw(VECTOR_SIZE, VECTOR_SIZE);
            busy.saw(VECTOR_SIZE, VECTOR_SIZE / 16);
        }

        assert!(!idle.worth(), "a filter that kept every row is not worth hashing for");
        assert!(busy.worth(), "one that kept a sixteenth of them is");

        // Giving up is final, so the rows that go past afterwards are never counted and the answer
        // cannot drift back.
        assert!(!idle.worth());
    }

    /// The line itself, which is a quarter of the rows dropped.
    #[test]
    fn a_filter_is_worth_hashing_for_once_it_drops_a_quarter_of_the_rows() {
        for (kept, worth) in [(0, true), (740, true), (749, true), (750, false), (1_000, false)] {
            let paying = Paying::default();
            for _ in 0..(WARMUP / 1_000 + 1) {
                paying.saw(1_000, kept);
            }
            assert_eq!(paying.worth(), worth, "{kept} of every thousand rows kept");
        }
    }

    /// The pushed filter counts as loose only after the warmup and only when it keeps more than
    /// half, which is when a Bloom filter goes ahead of it.
    #[test]
    fn a_pushed_filter_is_loose_once_it_keeps_more_than_half() {
        for (kept, loose) in [(0, false), (250, false), (500, false), (501, true), (1_000, true)] {
            let passed = Paying::default();
            assert!(!passed.loose(), "nothing is decided under the warmup");
            for _ in 0..(WARMUP / 1_000 + 1) {
                passed.saw(1_000, kept);
            }
            assert_eq!(passed.loose(), loose, "{kept} of every thousand rows kept");
        }
    }

    /// A join that never armed one, and a build side with no rows, both leave the scan reading
    /// everything. This is the case that must not regress, because every join that cannot use a
    /// runtime filter still makes one.
    #[test]
    fn a_scan_under_a_join_that_offered_nothing_reads_every_chunk() {
        let table = counted(VECTOR_SIZE * 3);
        let mut scan = scanning(&table, Vec::new());
        scan.sideways = Some(Sideways::new());

        assert_eq!(counted_rows(&scan), VECTOR_SIZE * 3);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 0);
    }

    /// Two conjuncts that between them leave no chunk, which is the shape ClickBench 37 has.
    #[test]
    fn conjuncts_that_rule_out_every_chunk_read_nothing() {
        let table = counted(VECTOR_SIZE * 4);
        let split = i128::try_from(VECTOR_SIZE * 2).expect("a small number");
        let scan = scanning(
            &table,
            vec![(0, Op::GreaterOrEqual, Bound::Int(split)), (0, Op::Less, Bound::Int(split))],
        );

        assert_eq!(counted_rows(&scan), 0);
        assert_eq!(scan.skipped.load(Ordering::Relaxed), 4);
    }

    /// The cutting of a row group, driven directly because every committed fixture is smaller than
    /// one morsel and so cannot be cut into more than one.
    #[test]
    fn the_morsels_of_a_row_group_tile_it_once_each() {
        for rows in [1, 2, 7, 9, 10, 100, 4_095, 4_096, 4_097, 122_880, 123_554] {
            for target in [1, 2, 3, 300, 4_096, 32_768] {
                let pieces = cutting(rows, target);
                assert_eq!(pieces.len(), parts(rows, target), "{rows} rows in morsels of {target}");
                let mut at = 0;
                for piece in &pieces {
                    assert_eq!(piece.start, at, "{rows} rows in morsels of {target}");
                    assert!(piece.len() <= target, "{rows} rows in morsels of {target}");
                    at = piece.end;
                }
                assert_eq!(at, rows, "{rows} rows in morsels of {target}");
            }
        }
    }

    /// Even morsels rather than full ones and a remainder, because the remainder is the morsel every
    /// other thread waits for.
    #[test]
    fn a_row_group_is_cut_into_even_morsels() {
        // A DuckDB row group in four, which is 30889 rows three times and 30887 once rather than
        // 32768 three times and 25250 once.
        let pieces = cutting(123_554, 32_768);
        assert_eq!(pieces.len(), 4);
        let longest = pieces.iter().map(std::ops::Range::len).max().expect("four morsels");
        let shortest = pieces.iter().map(std::ops::Range::len).min().expect("four morsels");
        assert_eq!(longest, 30_889);
        assert_eq!(shortest, 30_887);
        assert!(longest - shortest < pieces.len(), "morsels of {longest} and {shortest} rows");
    }

    /// The common case, which is every row group of every file small enough not to be worth cutting.
    #[test]
    fn a_row_group_no_larger_than_a_morsel_is_one_morsel() {
        assert_eq!(cutting(2_048, 32_768), vec![0..2_048]);
        assert_eq!(cutting(32_768, 32_768), vec![0..32_768]);
        assert_eq!(parts(2_048, 32_768), 1);
    }

    /// What stops a file with an empty row group in it from being cut forever. The group is counted
    /// as one morsel by `parts` and handed out as none, which is the safe way round because the
    /// count is only ever used to decide how many copies of a pipeline to build.
    #[test]
    fn an_empty_row_group_is_no_morsels() {
        assert!(next_piece(0, 0, 32_768).is_empty());
        assert_eq!(cutting(0, 32_768), Vec::new());
        assert_eq!(cutting(0, 0), Vec::new());
    }

    /// What a load asks a scan to gather up to. See `NativeSink` in the `rudb` crate.
    const LOAD: usize = 131_072;

    /// A scan with one thread that a load asked to gather takes the fixture's two groups as one
    /// morsel. The rows are the same rows in the same order, and the row numbers carry on across the
    /// join between the groups.
    #[test]
    fn a_scan_a_load_asked_to_gather_takes_small_row_groups_as_one_morsel() {
        let scan = untold(Vec::new());
        scan.gather(LOAD);
        assert_eq!(scan.morsels(1, 0), Some(1));
        assert_eq!(morsels(&scan), [4096], "both groups in one morsel");
        assert_eq!(morsels(&fixture()), [2048, 2048], "and apart when there are two threads");
    }

    /// A query asks for nothing and gets a morsel a group, however few threads it has.
    #[test]
    fn a_scan_nobody_asked_to_gather_hands_out_a_group_at_a_time() {
        let scan = untold(Vec::new());
        scan.gather(0);
        assert_eq!(scan.morsels(1, 0), Some(2));
        assert_eq!(morsels(&scan), [2048, 2048]);
    }

    /// A run stops at a group the filter rules out, and the next morsel starts past it.
    #[test]
    fn a_gathered_run_stops_at_a_group_the_filter_rules_out() {
        let scan = untold(vec![(0, Op::Greater, Bound::Int(1_000))]);
        scan.gather(LOAD);
        assert_eq!(scan.morsels(1, 0), Some(1));
        assert_eq!(morsels(&scan), Vec::<usize>::new());
        assert_eq!(scan.cutting.lock().expect("the lock holds").skipped, 2);
    }

    /// Groups join a run while it stays within the target, and a group as large as the target is
    /// never joined to anything.
    #[test]
    fn small_row_groups_are_gathered_up_to_the_target_and_large_ones_are_left_alone() {
        assert_eq!(gathered(8_000, [8_000, 8_000, 8_000].into_iter(), 20_000), 2);
        assert_eq!(gathered(8_000, [8_000; 40].into_iter(), LOAD), 16);
        assert_eq!(gathered(122_880, [122_880].into_iter(), LOAD), 1, "a DuckDB file");
        assert_eq!(gathered(8_000, [200_000, 8_000].into_iter(), LOAD), 1);
        assert_eq!(gathered(8_000, std::iter::empty(), LOAD), 1);
        assert_eq!(gathered(0, [0, 0].into_iter(), 0), 3, "empty groups cost nothing to join");
    }

    /// The ClickBench sample: 1,203 groups of about eight thousand rows. At thirty two threads it
    /// comes to about eighty morsels, the same as the ten million rows DuckDB writes in 81 groups,
    /// and never to fewer morsels than threads.
    #[test]
    fn the_clickbench_sample_is_gathered_into_as_many_morsels_as_duckdb_writes_groups() {
        let rows = vec![8_312; 1_203];
        let total: usize = rows.iter().sum();
        assert_eq!(morsels_of(&rows, 0, 0), 1_203, "one a group without gathering");
        assert_eq!(gather_target(total, 32, 0), 0, "a query asks for nothing");
        let gather = gather_target(total, 32, LOAD);
        assert_eq!(gather, LOAD);
        assert_eq!(morsels_of(&rows, 0, gather), 1_203_usize.div_ceil(15));
        let many = gather_target(total, 1_000, LOAD);
        assert!(morsels_of(&rows, 0, many) >= 1_000.min(rows.len()) / 2, "threads are kept busy");
        assert_eq!(gather_target(total, 2_000, LOAD), 4_999, "a share each, below a group");
        assert_eq!(morsels_of(&rows, 0, gather_target(total, 2_000, LOAD)), 1_203);
    }

    /// A file whose pages are as long as its row groups, which is every file DuckDB writes. The cut
    /// is turned off rather than made small, because a morsel that starts inside a page decodes that
    /// page whole and then throws most of it away.
    #[test]
    fn a_row_group_that_is_not_worth_cutting_is_one_morsel() {
        assert_eq!(cutting(123_554, 0), vec![0..123_554]);
        assert_eq!(parts(123_554, 0), 1);
        assert_eq!(parts(0, 0), 1);
    }

    /// How finely a file is cut, as a function of the threads there are to keep busy. Driven through
    /// the arithmetic rather than through a scan, because a committed fixture is one row group of
    /// 2048 rows and the question is about a file of nine groups of a hundred and twenty thousand.
    #[test]
    fn a_file_is_cut_only_as_finely_as_there_are_threads_to_want_it() {
        // A million rows in nine row groups, which is what DuckDB writes, with pages small enough
        // that cutting is allowed at all.
        // Sixty four threads want eight pieces of each group and get four, because `MORSEL_ROWS` is
        // the floor on how small a piece is worth being whatever the machine has.
        let rows = 123_554;
        let whole = 12_355_400;
        for (threads, wanted) in [(1, 1), (8, 1), (9, 1), (16, 2), (32, 4), (64, 4)] {
            let cut = cut_rows(1024, whole, rows, 9, threads);
            assert_eq!(parts(rows, cut), wanted, "{threads} threads over nine row groups");
        }
    }

    /// The two measured points are the hundred thousand row line and the million row line, and both
    /// of them are here rather than only in the comment, because a constant with a benchmark behind
    /// it should fail a test when somebody changes it without running one.
    #[test]
    fn native_workers_grow_with_the_rows_there_are_to_divide() {
        for (rows, workers) in [
            (0, 1),
            (1_000, 1),
            (25_000, 1),
            (50_000, 2),
            (100_000, 4),
            (200_000, 8),
            (500_000, 8),
            (1_000_000, 16),
            (2_500_000, 40),
            (9_999_750, 160),
        ] {
            assert_eq!(instances_for(rows, 1), workers, "{rows} rows");
        }
    }

    /// The same rule read the other way, where the rows are few and what is done to each is not.
    ///
    /// ClickBench 39 is the first row of this. It prunes to about twenty four thousand rows and
    /// its aggregate groups five columns, two of them wide strings, so a row costs the pipeline
    /// about a dozen times what reading it cost, and at a weight of one the whole query ran on one
    /// thread of thirty two.
    #[test]
    fn native_workers_grow_with_the_work_behind_each_row() {
        for (rows, weight, workers) in [
            (24_576, 1, 1),
            (24_576, 2, 2),
            (24_576, 7, 7),
            (24_576, 12, 8),
            (1_000, 25, 1),
            (3_000, 9, 2),
            (0, 100, 1),
        ] {
            assert_eq!(instances_for(rows, weight), workers, "{rows} rows at weight {weight}");
        }
    }

    /// A weight cannot make a large query wider, because how far a large query is worth dividing
    /// is a fact about the machine and the measurements say sixteen at a million rows.
    ///
    /// The small slope caps at eight, so any query with enough rows to reach eight on that slope
    /// alone is already there and there is nothing for a weight to add. That is everything from
    /// two hundred thousand live rows up, which is where the whole of this change stops applying.
    #[test]
    fn a_weight_never_moves_the_slope_that_divides_a_large_query() {
        for rows in [200_000, 500_000, 1_000_000, 2_500_000, 9_999_750] {
            let plain = instances_for(rows, 1);
            for weight in [2, 4, 12, 64] {
                assert_eq!(instances_for(rows, weight), plain, "{rows} rows at weight {weight}");
            }
        }
    }

    /// The other half of the rule. However many threads are waiting, a file whose first page costs
    /// a morsel more than a quarter of what the morsel reads is handed out a row group at a time.
    #[test]
    fn a_file_of_coarse_pages_is_not_cut_however_many_threads_there_are() {
        // A hundred bytes a row, so the finest morsel this rule makes of a group of 123_554 rows is
        // `MORSEL_ROWS` rows and 3_276_800 bytes, a quarter of which is 819_200.
        let rows = 123_554;
        let whole = 12_355_400;
        assert_eq!(cut_rows(whole, whole, rows, 9, 64), 0, "one page per chunk, as DuckDB writes");
        assert_eq!(cut_rows(819_201, whole, rows, 9, 64), 0);
        assert_eq!(cut_rows(0, whole, rows, 9, 64), 0);
        assert_eq!(cut_rows(1024, 0, rows, 9, 64), 0, "a group of no bytes is never worth cutting");
        // The largest first page the rule lets through at the finest cut it makes.
        assert!(cut_rows(819_200, whole, rows, 9, 64) > 0);
    }
}
