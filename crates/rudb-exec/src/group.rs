//! Grouping and duplicate elimination.
//!
//! Both are hash tables over [`Key`], which is what makes them agree about what one row is. A
//! `GROUP BY x` that put two nulls in two groups and a `SELECT DISTINCT x` that collapsed them into
//! one would be two answers to the same question, and the only way to be sure that never happens is
//! for both to ask the same type.
//!
//! Grouping goes through [`Table`], which is a hash table from a row of key columns to a slot, and
//! the slot is the number of groups that were seen before this one, so the output comes out in the
//! order the groups were first seen. SQL does not promise that and DuckDB does not either, but a
//! deterministic order costs nothing here and makes a failing test a diff instead of an
//! investigation.
//!
//! The state of every group lives in flat vectors indexed by that slot rather than in a vector of
//! its own, so a group that arrives costs a push and not a trip to the allocator, and the key of a
//! row that is not a new group is never copied anywhere at all. What is left per row is the probe,
//! with the hash of the whole chunk taken a column at a time before the row loop starts, which is
//! what #237 was about.
//!
//! `DISTINCT` is still a `HashSet<Key>` per group per call, which is the one place left where a row
//! is built to be asked about. It is asked about once per row, so it matters, and it is not this
//! change because a set per group is a different shape from a table over the whole input.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use rudb_common::{
    Error, Field, LogicalType, Memory, Reservation, Result, Session, Stage, Value, stage,
};
use rudb_kernels::{Accumulator, NOWHERE, is_true, settle_extremes, update_scattered};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Data, Form, VECTOR_SIZE, Validity, Vector};

use crate::buffer::Buffered;
use crate::group_distinct;
use crate::group_mixed;
use crate::key::{BigIntSet, Key, RowSet, mix, spread};
use crate::pairs::together;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::signed::SignedBlock;
use crate::spill::{Reader, Spill};
use crate::table::{Probe, Table, Walk};

/// One aggregate call, taken apart once when the operator is built.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<ExprRef>,
    distinct: bool,
    filter: Option<ExprRef>,
    returns: LogicalType,
    affine: Option<(usize, i64)>,
}

/// The `INTEGER` literal an expression is, and `None` for everything else.
///
/// Its own function so that the loop below holds no `Value::` at all. A pattern and a construction
/// are the same text, the row loop lint reads text, and the loop below runs once per aggregate call
/// in the plan rather than once per row. Hiding that from the lint with the deliberate marker would
/// be claiming the loop is row at a time, which it is not.
fn integer_constant(plan: &Plan, expr: ExprRef) -> Option<i64> {
    let Expr::Constant(value) = *plan.expr(expr) else { return None };
    let Value::Integer(offset) = *plan.value(value) else { return None };
    Some(i64::from(offset))
}

/// Marks `sum(SMALLINT + INTEGER literal)` calls that can reuse an earlier sum of the same column.
fn mark_affine_sums(plan: &Plan, calls: &mut [Call]) {
    for at in 0..calls.len() {
        if calls[at].name != "sum" || calls[at].distinct || calls[at].filter.is_some() {
            continue;
        }
        let [argument] = calls[at].args.as_slice() else { continue };
        let Expr::Function { name, args } = *plan.expr(*argument) else { continue };
        if plan.string(name) != "+" || plan.expr_type(*argument) != &LogicalType::Integer {
            continue;
        }
        let [left, right] = plan.expr_list(args) else { continue };
        let Expr::Cast { input: base, try_cast: false } = *plan.expr(*left) else { continue };
        if plan.expr_type(base) != &LogicalType::SmallInt {
            continue;
        }
        let Some(offset) = integer_constant(plan, *right) else { continue };
        let source = (0..at).find(|&source| {
            calls[source].name == "sum"
                && !calls[source].distinct
                && calls[source].filter.is_none()
                && calls[source].returns == LogicalType::HugeInt
                && calls[source].args.as_slice() == [base]
        });
        if let Some(source) = source {
            calls[at].affine = Some((source, offset));
        }
    }
}

/// A grouped or ungrouped aggregation.
///
/// The output is the group expressions followed by the aggregates, which is what a binding into
/// this operator's table index means and what the binder assumed when it made one.
///
/// An ungrouped aggregate produces exactly one row even over an empty input. That is done by
/// creating the single empty group when the instance is created rather than when the first row
/// arrives, which is the whole of the difference between `SELECT count(*) FROM empty` answering
/// zero and answering nothing.
///
/// # Radix partition ownership
///
/// A grouped aggregate hashes a chunk once and divides its rows by the high six hash bits. Each of
/// the sixty four partitions owns one table behind its own lock. Workers can update different
/// tables together, while equal keys always reach the same table and are stored once. This avoids
/// both the duplicate table memory and the second probe that a merge of per-worker tables requires.
///
/// The rows of a partition are gathered into typed vectors before they are folded. That keeps the
/// existing column kernels and table probe intact. An ungrouped aggregate and a grouping under a
/// pushed limit keep the single local table path because neither benefits from partitioning.
#[derive(Debug)]
pub(crate) struct Aggregate<'a> {
    plan: &'a Plan,
    /// Group expressions that vary by row and therefore belong in the physical key.
    keys: Vec<ExprRef>,
    groups: Vec<ExprRef>,
    /// Constant output group values, aligned with `groups`.
    constants: Vec<Option<Value>>,
    calls: Vec<Call>,
    inputs: Prepared,
    schema: Schema,
    /// Whether there are no group expressions, so every row goes to the one slot.
    alone: bool,
    /// Whether any call is `DISTINCT`, and so whether the sets that answer that are built at all. A
    /// group by with a million groups and no `DISTINCT` anywhere in it used to allocate a million
    /// empty sets to look at none of them.
    sets: bool,
    /// Which calls fold a vector at a time. An ungrouped aggregate has exactly one slot, so there
    /// is no key to build, no hash to take and no lookup to do, and what is left of the row loop is
    /// the fold itself. `DISTINCT` needs a value per row to put in a set and `FILTER` needs the rows
    /// it kept, and neither has a vector form yet, so a call with either stays on the row loop while
    /// the calls beside it do not.
    by_vector: Vec<bool>,
    /// Whether every call folds a vector at a time, which is when the row loop is skipped whole.
    every: bool,
    /// A grouped `count(*)` needs one integer per group rather than a general aggregate state.
    count_only: bool,
    /// COUNT(*), SUM(SMALLINT), AVG(SMALLINT) share one compact state per group.
    compact_numeric: bool,
    /// The only call is COUNT(DISTINCT BIGINT), whose completed state is ordered like COUNT(*).
    distinct_count: bool,
    /// SUM(SMALLINT), COUNT(*), AVG(SMALLINT), and COUNT(DISTINCT BIGINT) grouped by INTEGER.
    mixed_numeric_distinct: bool,
    /// An ungrouped COUNT(DISTINCT BIGINT) can exchange integer rows directly and count one set per
    /// radix owner instead of building and merging one general aggregate state per worker.
    radix_distinct_count: bool,
    /// Emit at most this many groups from each radix partition when the parent orders by count
    /// descending. The ordinary TopN still makes the final global choice.
    /// How many groups a count descending TopN above can observe, and which call it ranks by.
    top_counts: Option<(usize, usize)>,
    /// Emit only groups whose COUNT(*) call at this index reaches the inclusive bound.
    ///
    /// The Filter remains above the aggregate and checks the predicate again. This only avoids
    /// materializing groups that cannot pass it, so a missed shape is slow and never changes an
    /// answer.
    having_count: Option<(usize, i64)>,
    /// The most groups an unordered limit above this operator can observe.
    max_groups: Option<usize>,
    /// The groups a pushed down limit keeps, agreed once and used by every instance.
    agreed: Mutex<Option<Agreed>>,
    /// Whether [`Agreed::keys`] is filled in, so the fold can ask without taking the lock.
    settled: AtomicBool,
    memory: Memory,
    /// The chunks the passes have finished, and what they are charged.
    built: Mutex<Built>,
    /// One final table per radix partition. Different workers can merge different partitions at
    /// the same time, and no final table spanning every group is needed.
    merged: Vec<Mutex<Partition>>,
    /// How many instances have started, which is how an instance knows whether partitioning could
    /// buy it anything at all.
    ///
    /// A heuristic and nothing more. It is read while the instances are still starting, so it can
    /// be low, and an instance that reads it low keeps its own table a little longer than it had to.
    /// That costs speed and never an answer, because what makes partitioning safe is the flag in
    /// [`Built`] and not this.
    started: AtomicUsize,
    /// A cheap read of [`Built::local`], so the fold does not take a lock to ask.
    ///
    /// It can be read stale, and reading it stale costs a chunk folded into a table that is about
    /// to be handed over rather than an answer. What decides the question is the field under the
    /// lock, which is checked again at the one moment it matters.
    locally: AtomicBool,
    /// A grouped count over one stable dictionary is a dense array indexed by its storage code.
    dense: OnceLock<DenseCount>,
    /// Fixed width rows exchanged to one owner per radix partition for compact count aggregates.
    fixed: OnceLock<FixedExchange>,
    /// Integer rows exchanged to one owner per radix partition for an ungrouped distinct count.
    bigint_distinct: OnceLock<BigIntDistinctExchange>,
    /// Native integer and dictionary-code rows exchanged for a three-key count and TopN.
    encoded_count: OnceLock<Option<EncodedCountExchange>>,
    /// Fixed group and BIGINT pairs exchanged for grouped distinct counts.
    grouped_distinct: OnceLock<Option<group_distinct::Exchange>>,
    /// Fixed rows exchanged for one mixed aggregate state per INTEGER group.
    mixed: OnceLock<group_mixed::Exchange>,
    out: Buffered,
}

#[derive(Debug)]
struct DenseCount {
    dictionary: Arc<Vector>,
    partitions: Vec<Mutex<DensePartition>>,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug, Default)]
struct DensePartition {
    runs: Vec<Vec<u32>>,
    nulls: i64,
}

#[derive(Debug)]
struct FixedExchange {
    /// The two key types, which the emit puts the record's two integers back into.
    ///
    /// A record holds the first key as eight bytes and the second as four whatever their columns
    /// were, because one width is what lets a radix partition be one type. These are what the
    /// widths came from and what they go back to.
    keys: [LogicalType; 2],
    partitions: Vec<Mutex<FixedRuns>>,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug)]
struct BigIntDistinctExchange {
    partitions: Vec<Mutex<BigIntDistinctRuns>>,
    held: Mutex<Vec<Reservation>>,
}

/// One radix partition's values, as the run each instance handed over rather than one flat run.
///
/// An instance that finishes used to append its values onto the shared run, which is a copy of every
/// value in the table except the first instance's, sixteen megabytes of it on a million rows, done
/// while holding the partition's lock. Handing the run over instead is a move, so the copy and the
/// time under the lock both go away, and the pass that counts the distinct values walks the runs one
/// after another and cannot tell the difference.
#[derive(Debug, Default)]
struct BigIntDistinctRuns {
    runs: Vec<Vec<i64>>,
}

#[derive(Debug, Default)]
struct BigIntDistinctPartition {
    rows: Vec<i64>,
}

impl BigIntDistinctPartition {
    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<i64>()
    }
}

#[derive(Debug)]
struct EncodedCountExchange {
    dictionary: Arc<Vector>,
    /// The types of the keys ahead of the string one, which the emit puts the values back into.
    ///
    /// A record holds every one of them as eight bytes whatever their columns were, because one
    /// width is what lets a radix partition be one type. These are what the width came from and
    /// what it goes back to.
    leading: Vec<LogicalType>,
    /// Whether the string key can be null through the dictionary rather than through its own mask.
    ///
    /// Settled when the exchange is built rather than per chunk, because it is the same dictionary
    /// for every chunk and the `URL` one on the ClickBench file holds half a million entries. A form
    /// that keeps its nulls somewhere other than its own mask counts as holding one, since the mask
    /// is then not the whole answer and the scatter has to ask the vector row by row.
    dictionary_nulls: bool,
    partitions: Vec<Mutex<EncodedCountRuns>>,
    held: Mutex<Vec<Reservation>>,
}

/// One radix partition's records, as the run each instance handed over rather than one flat run.
///
/// The same move [`BigIntDistinctRuns`] is, for the same reason and with the same saving. An
/// instance used to append its records onto the shared run while holding the partition's lock, which
/// on the million row ClickBench file is twenty two of the twenty four megabytes it scattered copied
/// a second time, with sixteen instances queueing behind one lock per partition to do it. Handing
/// the run over is a move, and the fold walks the runs one after another instead of one flat array.
#[derive(Debug, Default)]
struct EncodedCountRuns {
    runs: Vec<EncodedCountPartition>,
}

/// One radix partition's records for the fixed exchange, one run per instance. See
/// [`EncodedCountRuns`], which this is the same thing as for a different record.
#[derive(Debug, Default)]
struct FixedRuns {
    runs: Vec<FixedPartition>,
}

impl EncodedCountRuns {
    /// The run the rest fold into, which is the widest so that the most records stay where they are,
    /// and how many records every run holds between them.
    fn seed(&mut self) -> (EncodedCountPartition, usize) {
        let total = self.runs.iter().map(|run| run.rows.len()).sum();
        let widest = widest_run(self.runs.iter().map(|run| run.rows.len()));
        let seed = widest.map(|at| self.runs.swap_remove(at)).unwrap_or_default();
        (seed, total)
    }
}

impl FixedRuns {
    /// The run the rest fold into, and how many records every run holds between them. See
    /// [`EncodedCountRuns::seed`].
    fn seed(&mut self) -> (FixedPartition, usize) {
        let total = self.runs.iter().map(|run| run.rows.len()).sum();
        let widest = widest_run(self.runs.iter().map(|run| run.rows.len()));
        let seed = widest.map(|at| self.runs.swap_remove(at)).unwrap_or_default();
        (seed, total)
    }
}

/// Which of these runs holds the most records, or `None` when there are no runs at all.
fn widest_run(lengths: impl Iterator<Item = usize>) -> Option<usize> {
    lengths.enumerate().max_by_key(|&(_, rows)| rows).map(|(at, _)| at)
}

/// How many of a bucket's bits hold the slot its group sits at, the rest being the tag.
///
/// Twenty four, which is sixteen million groups in one radix partition and a billion across the
/// sixty four. A partition that outgrows it says so rather than wrapping, and the ClickBench file
/// would have to be hundreds of times larger before one did.
const SLOT_BITS: u32 = 24;

/// The part of a bucket that is the slot.
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

/// The bucket value that says a slot in a radix partition's open addressed table is free.
///
/// Every slot bit set and no tag bits, so a free bucket is a single comparison against the slot
/// half and never has to be told apart from a real entry that happens to share its tag.
const EMPTY_SLOT: u32 = SLOT_MASK;

/// The eight bits of a group's hash that its bucket carries beside the slot.
///
/// This is what makes the probe one random read instead of two. Without it a bucket says only where
/// its group is, so the only way to find out whether it is the right group is to read the group,
/// and the array of groups is megabytes and read in an order the hash chose. With it, two hundred
/// and fifty five mismatches in two hundred and fifty six are rejected inside the bucket array,
/// which is a quarter the size and which the probe has already touched. It is free in memory
/// because the slot never needed more than twenty four of the thirty two bits it was given.
///
/// The eight bits above the ones the bucket index uses, because a tag cut from bits the index
/// already used would be the same for every bucket in a chain and would reject nothing. A partition
/// cannot hold more buckets than [`SLOT_BITS`] allows slots, so those eight are always above it.
/// One of the two records hashes to a `u32` and the other to a `u64`, and both are widened here so
/// that the tag is the same bits of whichever it is.
const fn slot_tag(hash: u64) -> u32 {
    (((hash >> SLOT_BITS) as u32) & 0xff) << SLOT_BITS
}

/// A bucket for a group at this slot with this hash, or an error when the partition is too large.
fn bucket_for(slot: usize, hash: u64, what: &'static str) -> Result<u32> {
    let slot = u32::try_from(slot).ok().filter(|&slot| slot < SLOT_MASK);
    let slot = slot.ok_or_else(|| Error::out_of_memory(what))?;
    Ok(slot_tag(hash) | slot)
}

#[derive(Debug, Clone, Copy)]
struct EncodedCountRecord {
    first: i64,
    second: i64,
    hash: u32,
    third: u32,
}

#[derive(Debug, Default)]
struct EncodedCountPartition {
    rows: Vec<EncodedCountRecord>,
    /// Empty while all three keys are valid. It is allocated when this partition sees a null.
    validity: Vec<u8>,
}

impl EncodedCountRecord {
    const FIRST: u8 = 1;
    const SECOND: u8 = 2;
    const THIRD: u8 = 4;
    const ALL: u8 = Self::FIRST | Self::SECOND | Self::THIRD;
}

impl EncodedCountPartition {
    /// Takes one record, and starts keeping validity if this is the first null this has seen.
    ///
    /// An empty validity vector means every record here is valid, so the moment one is not the
    /// vector has to say so for all of them by name. The condition is whether validity is being kept
    /// at all rather than whether it is empty, because the first record a partition ever sees can
    /// itself be the null one, and there is nothing to fill in behind it.
    fn push(&mut self, row: EncodedCountRecord, valid: u8) {
        let keeping = !self.validity.is_empty() || valid != EncodedCountRecord::ALL;
        if keeping {
            self.validity.resize(self.rows.len(), EncodedCountRecord::ALL);
        }
        self.rows.push(row);
        if keeping {
            self.validity.push(valid);
        }
    }

    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<EncodedCountRecord>()
            + self.validity.capacity() * size_of::<u8>()
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedRecord {
    first: i64,
    second: i32,
    sum: i16,
    mean: i16,
}

#[derive(Debug, Default)]
struct FixedPartition {
    rows: Vec<FixedRecord>,
    /// Empty while every field is valid. It is materialized only when this partition sees a null.
    validity: Vec<u8>,
}

impl FixedRecord {
    const FIRST: u8 = 1;
    const SECOND: u8 = 2;
    const SUM: u8 = 4;
    const MEAN: u8 = 8;
    const ALL: u8 = Self::FIRST | Self::SECOND | Self::SUM | Self::MEAN;
}

/// The two group keys' hash. Fixed records leave this cheap derived word out so that ten million
/// exchanged rows occupy 160 MB rather than 240 MB. It is needed once to choose an owner and once
/// when that owner builds its table; carrying it between those two points costs more bandwidth than
/// the two integer mixes cost to repeat.
fn fixed_hash(row: FixedRecord, valid: u8) -> u64 {
    const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;
    let first = if valid & FixedRecord::FIRST != 0 { row.first as u64 } else { NOTHING };
    let second =
        if valid & FixedRecord::SECOND != 0 { i64::from(row.second) as u64 } else { NOTHING };
    spread(mix(mix(0, first), second))
}

impl FixedPartition {
    /// Takes one record, keeping validity from the first null this partition sees. The same shape as
    /// [`EncodedCountPartition::push`], and null on the first record for the same reason.
    fn push(&mut self, row: FixedRecord, valid: u8) {
        let keeping = !self.validity.is_empty() || valid != FixedRecord::ALL;
        if keeping {
            self.validity.resize(self.rows.len(), FixedRecord::ALL);
        }
        self.rows.push(row);
        if keeping {
            self.validity.push(valid);
        }
    }

    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<FixedRecord>() + self.validity.capacity() * size_of::<u8>()
    }
}

/// What the passes have finished, which is the answer as it is assembled.
#[derive(Debug)]
struct Built {
    chunks: Vec<Chunk>,
    /// What those chunks are charged, held for as long as they are readable.
    ///
    /// One per partition rather than one in total, because the partitions are finished on separate
    /// threads and a reservation belongs to the thread growing it. Moving a charge per partition
    /// into one at the end would mean holding both charges for as long as the move took,
    /// and the thing being charged for here is the whole answer.
    held: Vec<Reservation>,
    /// How many instances have combined, which is one per thread the pipeline ran on.
    instances: usize,
    /// Whether the groups now live in the partitions rather than in one table per instance.
    ///
    /// Read and written only under this lock, and that is what makes the switch safe. Once it is
    /// true every table has to be scattered, including one belonging to an instance that never grew
    /// large enough to switch on its own, because a group that is in `merged[0]` whole and in
    /// `merged[5]` as part of a partition comes out of `finalize` twice.
    partitioning: bool,
    /// Whether a partitioned instance still keeps its own table per partition.
    ///
    /// True until something makes it impossible, which is one of the two spilling cases. Once it is
    /// false it never becomes true again, every instance folds into the shared tables instead, and
    /// the tables already handed in are folded in too before this lock is let go. That ordering is
    /// the whole of why it is safe: an instance takes this lock before it hands a table in, so it
    /// cannot be doing that while the switch is happening.
    local: bool,
}

/// One radix partition: the groups that hash to it, and the ones that arrived too late to join them.
///
/// The second half is what [`Aggregate::scatter`] leaves behind. A table that has started spilling
/// sends every key it does not already hold to its file, so inserting a scattered group into it
/// would put that group in the table and its rows in the file at once, and the group would come out
/// of `finalize` twice. Those groups wait here instead and are folded into the pass that reads the
/// file back, where their rows are, which is the one place they can meet without being counted
/// twice.
///
/// Empty on every ordinary query. It takes an aggregate that spills before its instances have
/// handed their groups over to reach it, which means a budget tight enough to crowd while the
/// instances are still small.
#[derive(Debug, Default)]
struct Partition {
    table: Option<Building>,
    carried: Option<Building>,
    /// The tables instances kept to themselves, waiting to be merged into one.
    ///
    /// One per instance that folded anything into this partition. They are merged by whichever
    /// thread closes this partition, which is one thread per partition and so as many merges
    /// running at once as there are partitions rather than one.
    pending: Vec<Building>,
}

/// How many radix partitions the groups are spread over.
///
/// This is the ceiling on how many threads can finish an aggregate, because a partition is finished
/// by one thread and nothing it holds depends on any other partition. It was sixteen, which was
/// enough while a pipeline borrowed threads for its source and a million row scan cut sixteen
/// morsels. Now that the borrow is the wider of the source and the sink, sixteen is what stops a
/// thirty two thread machine from finishing on thirty two threads, and it has to be at least as
/// large as the thread count for the finish to reach the whole machine.
///
/// Sixty four rather than thirty two, so that a machine larger than this one is covered and so that
/// a thread that draws a heavy partition is one of four a thread has rather than one of one. What
/// more partitions cost is the scatter: a chunk is split into one set of vectors per partition and
/// sixty four of them are smaller pieces than sixteen. On `GROUP BY WatchID, ClientIP` over a
/// million rows, where the groups are nearly one per row and this is the whole query, the merge
/// halves and the scatter does not move enough to take it back.
const RADIX_PARTITIONS: usize = 64;
const DENSE_PARTITIONS: usize = 4;

/// How many groups an instance holds before it stops keeping them to itself.
///
/// Partitioning is not free. Every chunk is hashed, split, and gathered into one set of vectors per
/// partition, which is a copy of every column it carries, and then a lock is taken per partition to
/// fold the pieces. On a small aggregate that is all cost: ClickBench at a thousand rows ran 41
/// percent slower and at ten thousand rows 19 percent slower when every grouped aggregate
/// partitioned from its first chunk, because none of those tables is large enough for the sharing
/// to pay for itself.
///
/// Four thousand is where the measurement put it. Sixteen thousand was tried first, on the argument
/// that it is where a table stops fitting comfortably in cache, and it left a five percent loss at a
/// million rows: an instance that holds sixteen thousand groups to itself is an instance the other
/// threads cannot help with. At four thousand, ClickBench on gamingpc-wsl runs 2.8 times faster at a
/// thousand rows, 1.8 times at ten thousand and 1.44 times at a million, all against main, on the
/// same peak memory. Both numbers were measured in the same sweep and the lower one won everywhere.
const PARTITION_FROM: usize = 4_096;

/// How much of one set of tables per instance the cache is taken to hold.
///
/// Read by [`Aggregate::cache_holds_local`], which is where the reasoning is. It is a constant
/// rather than a reading off the machine because nothing portable reports a cache size, and it is
/// the last level that matters here rather than a core's own, since the instances are sharing it.
/// Sixteen megabytes is what the laptop this was measured on holds and is the same order as every
/// desktop and server part worth running this on, so a machine with more cache keeps its tables
/// private slightly later than it could and a machine with less shares slightly later than it
/// should. Both of those are a few per cent on an aggregate near the line and nothing at all on one
/// far from it.
const LOCAL_CACHE: u64 = 16 * 1024 * 1024;

impl<'a> Aggregate<'a> {
    /// Applies the session semantics to group keys and aggregate inputs.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.inputs = self.inputs.in_session(session);
        self
    }

    /// An aggregation over the plan's groups and aggregate calls, and the source it finishes into.
    ///
    /// # Errors
    ///
    /// If an entry in the aggregate list is not an aggregate, which [`Plan::validate`] rejects and
    /// which is checked again here because this operator has no sensible behaviour if it is wrong.
    pub(crate) fn new(
        plan: &'a Plan,
        input: &Schema,
        index: u32,
        groups: Slice,
        aggregates: Slice,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        let input_schema = input.clone();
        let groups: Vec<ExprRef> = plan.expr_list(groups).to_vec();
        let constants: Vec<Option<Value>> = groups
            .iter()
            .map(|&group| match *plan.expr(group) {
                Expr::Constant(value) => Some(plan.value(value).clone()),
                _ => None,
            })
            .collect();
        let keys: Vec<ExprRef> = groups
            .iter()
            .zip(&constants)
            .filter_map(|(&group, value)| value.is_none().then_some(group))
            .collect();
        let mut calls = Vec::new();
        for &reference in plan.expr_list(aggregates) {
            let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(reference) else {
                return Err(Error::internal(format!(
                    "expression {reference} is in the aggregate list of an Aggregate and is not an aggregate"
                )));
            };
            calls.push(Call {
                name: plan.string(name).to_string(),
                args: plan.expr_list(args).to_vec(),
                distinct,
                filter,
                returns: plan.expr_type(reference).clone(),
                affine: None,
            });
        }
        if groups.is_empty() {
            mark_affine_sums(plan, &mut calls);
        }
        let mut fields = Vec::with_capacity(groups.len() + calls.len());
        for (at, &group) in groups.iter().enumerate() {
            fields.push(Field::new(
                group_name(plan, group, &input_schema, at),
                plan.expr_type(group).clone(),
            ));
        }
        for call in &calls {
            fields.push(Field::new(call.name.clone(), call.returns.clone()));
        }
        let schema = Schema::numbered(fields, index);
        let mut inputs = keys.clone();
        for call in &calls {
            if call.affine.is_none() {
                inputs.extend_from_slice(&call.args);
            }
            inputs.extend(call.filter);
        }
        let inputs = Prepared::shared(plan, &inputs, &input_schema)?;
        let alone = groups.is_empty();
        let compact_numeric = !alone
            && calls.len() == 3
            && calls[0].name == "count_star"
            && calls[0].args.is_empty()
            && calls[1].name == "sum"
            && calls[1].args.len() == 1
            && plan.expr_type(calls[1].args[0]) == &LogicalType::SmallInt
            && calls[1].returns == LogicalType::HugeInt
            && calls[2].name == "avg"
            && calls[2].args.len() == 1
            && plan.expr_type(calls[2].args[0]) == &LogicalType::SmallInt
            && calls[2].returns == LogicalType::Double
            && calls.iter().all(|call| !call.distinct && call.filter.is_none());
        let distinct_count = !alone
            && calls.len() == 1
            && calls[0].name == "count"
            && calls[0].distinct
            && calls[0].args.len() == 1
            && plan.expr_type(calls[0].args[0]) == &LogicalType::BigInt
            && calls[0].filter.is_none();
        let mixed_numeric_distinct = !alone
            && calls.len() == 4
            && calls[0].name == "sum"
            && !calls[0].distinct
            && calls[0].args.len() == 1
            && plan.expr_type(calls[0].args[0]) == &LogicalType::SmallInt
            && calls[0].returns == LogicalType::HugeInt
            && calls[1].name == "count_star"
            && !calls[1].distinct
            && calls[1].args.is_empty()
            && calls[1].returns == LogicalType::BigInt
            && calls[2].name == "avg"
            && !calls[2].distinct
            && calls[2].args.len() == 1
            && plan.expr_type(calls[2].args[0]) == &LogicalType::SmallInt
            && calls[2].returns == LogicalType::Double
            && calls[3].name == "count"
            && calls[3].distinct
            && calls[3].args.len() == 1
            && plan.expr_type(calls[3].args[0]) == &LogicalType::BigInt
            && calls[3].returns == LogicalType::BigInt
            && calls.iter().all(|call| call.filter.is_none());
        let radix_distinct_count = alone
            && calls.len() == 1
            && calls[0].name == "count"
            && calls[0].distinct
            && calls[0].args.len() == 1
            && plan.expr_type(calls[0].args[0]) == &LogicalType::BigInt
            && calls[0].filter.is_none();
        let by_vector: Vec<bool> =
            calls.iter().map(|call| alone && !call.distinct && call.filter.is_none()).collect();
        let out = Buffered::new();
        let aggregate = Self {
            plan,
            keys,
            constants,
            alone,
            sets: calls.iter().any(|call| call.distinct),
            every: by_vector.iter().all(|&yes| yes),
            count_only: !alone
                && calls.len() == 1
                && calls[0].name == "count_star"
                && !calls[0].distinct
                && calls[0].filter.is_none(),
            compact_numeric,
            distinct_count,
            mixed_numeric_distinct,
            radix_distinct_count,
            top_counts: None,
            having_count: None,
            max_groups: None,
            agreed: Mutex::new(None),
            settled: AtomicBool::new(false),
            by_vector,
            groups,
            calls,
            inputs,
            schema,
            memory: memory.clone(),
            built: Mutex::new(Built {
                chunks: Vec::new(),
                held: Vec::new(),
                instances: 0,
                partitioning: false,
                local: true,
            }),
            merged: (0..RADIX_PARTITIONS).map(|_| Mutex::new(Partition::default())).collect(),
            started: AtomicUsize::new(0),
            locally: AtomicBool::new(true),
            dense: OnceLock::new(),
            fixed: OnceLock::new(),
            bigint_distinct: OnceLock::new(),
            encoded_count: OnceLock::new(),
            grouped_distinct: OnceLock::new(),
            mixed: OnceLock::new(),
            out: out.clone(),
        };
        Ok((aggregate, out))
    }

    /// Stops opening groups once an unordered limit above this aggregate cannot observe another.
    pub(crate) fn limit_groups(mut self, max_groups: usize) -> Self {
        self.max_groups = Some(max_groups);
        self
    }

    /// Keeps only the groups that can still reach a count-descending TopN above this aggregate.
    ///
    /// The counting shapes answer the count from their own state and not from an accumulator, so
    /// which call it is does not reach them and they keep passing zero. An aggregate holding one
    /// accumulator per call needs the call the TopN sorts on, and takes this only for a plain
    /// `COUNT(*)`, which is the one whose running value a group is certain to be holding.
    ///
    /// Dropping the groups that cannot be reached is worth more here than on the counting shapes,
    /// because everything else in the row is built per group at the end: a `MIN` over a string
    /// column fetches a value out of the dictionary for each group, and a million of those thrown
    /// away by a `LIMIT 10` above is a read of most of the column for nothing.
    #[must_use]
    pub(crate) fn top_counts(mut self, bound: usize, call: usize) -> Self {
        if self.count_only
            || self.compact_numeric
            || self.distinct_count
            || self.mixed_numeric_distinct
        {
            self.top_counts = Some((bound, 0));
        } else if self.calls.get(call).is_some_and(|call| {
            call.name == "count_star"
                && call.args.is_empty()
                && !call.distinct
                && call.filter.is_none()
        }) {
            self.top_counts = Some((bound, call));
        }
        self
    }

    /// Drops groups below an inclusive COUNT(*) bound before result vectors are materialized.
    #[must_use]
    pub(crate) fn having_count(mut self, call: usize, minimum: i64) -> Self {
        if self.calls.get(call).is_some_and(|call| {
            call.name == "count_star"
                && call.args.is_empty()
                && !call.distinct
                && call.filter.is_none()
        }) {
            self.having_count = Some((call, minimum));
        }
        self
    }

    /// What this operator produces, which is the group expressions followed by the aggregates.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// One input chunk, with the group keys, the arguments and the filters evaluated out of it.
    ///
    /// The same shape a spill file is read back into, which is what lets one row loop serve a chunk
    /// that was pushed and a chunk that was written out and read again.
    fn read(&self, chunk: &Chunk, scratch: &mut Scratch) -> Result<Rows> {
        let rows = chunk.len();
        let mut evaluated = Vec::new();
        self.inputs.evaluate(chunk, scratch, &mut evaluated)?;
        let mut values = evaluated.into_iter();
        let keys = values.by_ref().take(self.keys.len()).collect();
        let mut arguments = Vec::with_capacity(self.calls.len());
        let mut filters = Vec::with_capacity(self.calls.len());
        for call in &self.calls {
            arguments.push(if call.affine.is_none() {
                values.by_ref().take(call.args.len()).collect()
            } else {
                Vec::new()
            });
            filters
                .push(call.filter.map(|_| values.next().expect("a prepared filter has a value")));
        }
        Ok(Rows { keys, arguments, filters, rows })
    }

    /// Whether the fixed width radix exchange can own this aggregate.
    ///
    /// The two keys are signed integers. The first can be any width, because the record holds it as
    /// eight bytes and the emit puts it back in the type the query asked for. The second has to fit
    /// in four, because that is what the record gives it and widening the field would cost every
    /// exchanged row four bytes to buy nothing. That is what lets `GROUP BY SearchEngineID,
    /// ClientIP` reach this: the first column is a `SMALLINT` and the second is already four bytes.
    fn fixed_top_count(&self) -> bool {
        self.compact_numeric
            && self.top_counts.is_some()
            && self.constants.iter().all(Option::is_none)
            && self.keys.len() == 2
            && signed_key(self.plan.expr_type(self.keys[0]))
            && narrow_key(self.plan.expr_type(self.keys[1]))
    }

    /// Whether the encoded count radix exchange can own this aggregate.
    ///
    /// The keys are one or two signed integers followed by a string. Any width of signed integer
    /// will do, not just `BIGINT`, because the record holds them all as eight bytes and the emit
    /// puts them back in the type the query asked for. That is what lets `GROUP BY SearchEngineID,
    /// SearchPhrase` reach this: the column is a `SMALLINT` and nothing about it needs eight bytes.
    fn encoded_top_count(&self) -> bool {
        if !self.count_only
            || self.top_counts.is_none()
            || !self.constants.iter().all(Option::is_none)
        {
            return false;
        }
        let Some((last, leading)) = self.keys.split_last() else { return false };
        (1..=2).contains(&leading.len())
            && self.plan.expr_type(*last) == &LogicalType::Varchar
            && leading.iter().all(|&key| signed_key(self.plan.expr_type(key)))
    }

    /// Whether the grouped distinct radix exchange can own this aggregate.
    ///
    /// A `VARCHAR` key is admitted alongside a signed integer one because a string column that
    /// arrives with a stable dictionary has a four byte code per row that picks out exactly the
    /// groups the strings do. Whether it does arrive that way is not known until a chunk turns up,
    /// so the type is all that is asked here and the exchange decides the rest on its first chunk.
    ///
    /// Two keys are admitted on the same terms, and whether two of them fit in the four bytes a
    /// record holds is also left to the first chunk, because the width of a string key is the width
    /// of its dictionary and nothing here knows that yet. `GROUP BY MobilePhone, MobilePhoneModel`
    /// is the shape this is for: a `SMALLINT` and a dictionary of under a hundred models fit side
    /// by side with room to spare, and an aggregate that does not fit falls through to the general
    /// table.
    fn grouped_distinct_top_count(&self) -> bool {
        self.distinct_count
            && self.top_counts.is_some()
            && self.constants.iter().all(Option::is_none)
            && (1..=2).contains(&self.keys.len())
            && self.keys.iter().all(|&key| {
                matches!(
                    self.plan.expr_type(key),
                    LogicalType::TinyInt
                        | LogicalType::SmallInt
                        | LogicalType::Integer
                        | LogicalType::Varchar
                )
            })
    }

    fn mixed_top_count(&self) -> bool {
        self.mixed_numeric_distinct
            && self.top_counts.is_some()
            && self.constants.iter().all(Option::is_none)
            && self.keys.len() == 1
            && self.plan.expr_type(self.keys[0]) == &LogicalType::Integer
    }

    fn buffer_fixed(
        &self,
        rows: &Rows,
        partitions: &mut [FixedPartition],
        memory: &mut Reservation,
        blocks: &mut FixedBlocks,
    ) -> Result<()> {
        self.fixed.get_or_init(|| FixedExchange {
            keys: [
                self.plan.expr_type(self.keys[0]).clone(),
                self.plan.expr_type(self.keys[1]).clone(),
            ],
            partitions: (0..RADIX_PARTITIONS).map(|_| Mutex::new(FixedRuns::default())).collect(),
            held: Mutex::new(Vec::new()),
        });
        let [first, second] = rows.keys.as_slice() else {
            return Err(Error::internal("a fixed radix exchange received the wrong key width"));
        };
        let sum = rows.arguments[1].first().expect("SUM has one argument");
        let mean = rows.arguments[2].first().expect("AVG has one argument");
        let before = partitions.iter().map(FixedPartition::footprint).sum::<usize>();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        // The four columns read once for the chunk rather than four times per row. See
        // [`FixedBlocks`] for what that was costing.
        blocks.read(rows.rows, [first, second, sum, mean])?;
        let [held_first, held_second, held_sum, held_mean] = blocks.cut(rows.rows)?;
        let [null_first, null_second, null_sum, null_mean] = blocks.nulled();
        for row in 0..rows.rows {
            let mut valid = 0;
            let first_value = if null_first && first.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::FIRST;
                held_first[row]
            };
            let second_value = if null_second && second.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::SECOND;
                i32::try_from(held_second[row])
                    .map_err(|_| Error::internal("a fixed second key is out of range"))?
            };
            let sum_value = if null_sum && sum.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::SUM;
                i16::try_from(held_sum[row])
                    .map_err(|_| Error::internal("a fixed SMALLINT sum is out of range"))?
            };
            let mean_value = if null_mean && mean.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::MEAN;
                i16::try_from(held_mean[row])
                    .map_err(|_| Error::internal("a fixed SMALLINT mean is out of range"))?
            };
            let record = FixedRecord {
                first: first_value,
                second: second_value,
                sum: sum_value,
                mean: mean_value,
            };
            let hash = fixed_hash(record, valid);
            partitions[(hash >> shift) as usize].push(record, valid);
        }
        let after = partitions.iter().map(FixedPartition::footprint).sum::<usize>();
        memory.grow(width_of(after.saturating_sub(before)))
    }

    fn buffer_encoded_count(
        &self,
        rows: &Rows,
        partitions: &mut [EncodedCountPartition],
        memory: &mut Reservation,
    ) -> Result<bool> {
        let (first, second, third) = match rows.keys.as_slice() {
            [first, third] => (first, None, third),
            [first, second, third] => (first, Some(second), third),
            _ => {
                return Err(Error::internal(
                    "an encoded count exchange received the wrong key width",
                ));
            }
        };
        let dictionary = third.stable_dictionary_parts();
        let state = self.encoded_count.get_or_init(|| {
            dictionary.as_ref().map(|(_, dictionary)| EncodedCountExchange {
                dictionary: Arc::clone(dictionary),
                leading: self.keys[..self.keys.len() - 1]
                    .iter()
                    .map(|&key| self.plan.expr_type(key).clone())
                    .collect(),
                dictionary_nulls: dictionary.validity().has_nulls(dictionary.len())
                    || !nulls_are_in_the_mask(dictionary),
                partitions: (0..RADIX_PARTITIONS)
                    .map(|_| Mutex::new(EncodedCountRuns::default()))
                    .collect(),
                held: Mutex::new(Vec::new()),
            })
        });
        let Some(state) = state else { return Ok(false) };
        let Some((codes, dictionary)) = dictionary else {
            return Err(Error::internal(
                "an encoded count exchange changed from dictionary to flat strings",
            ));
        };
        if !Arc::ptr_eq(&state.dictionary, dictionary) {
            return Err(Error::internal(
                "an encoded count exchange received two string code spaces",
            ));
        }
        if state.leading.len() + 1 != rows.keys.len() {
            return Err(Error::internal("an encoded count exchange changed key width"));
        }
        let before = partitions.iter().map(EncodedCountPartition::footprint).sum::<usize>();
        let shift = u32::BITS - RADIX_PARTITIONS.ilog2();
        const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;
        // Every key of this chunk read the way the chunk holds it, once, and `None` as soon as one
        // of them has a null in it or is in a form the run reader does not cover. The loop below
        // that covers every form and every null is still there and still right, and this is the same
        // lift #237 did for the group hash, #539 for the key comparison and #800 for the distinct
        // scatter: what a row at a time reader does per row is mostly deciding what it is reading.
        let plain = Signed::of(first, rows.rows).zip(match second {
            Some(second) => Signed::of(second, rows.rows).map(Some),
            None => Some(None),
        });
        let plain = plain.filter(|_| {
            !state.dictionary_nulls
                && third.len() >= rows.rows
                && !third.validity().has_nulls(rows.rows)
        });
        if let Some((first, second)) = plain {
            for (row, &third_code) in codes.iter().enumerate().take(rows.rows) {
                if third_code as usize >= dictionary.len() {
                    return Err(Error::internal("an encoded string code is out of range"));
                }
                let first_value = first.at(row);
                // Zero rather than the stand-in for a null when there is no second key, which is
                // what the row at a time loop hashes for an absent one, so both agree about a group.
                let second_value = second.map_or(0, |second| second.at(row));
                let wide = spread(mix(
                    mix(mix(0, first_value as u64), second_value as u64),
                    u64::from(third_code),
                ));
                let hash = (wide ^ (wide >> 32)) as u32;
                partitions[(hash >> shift) as usize].push(
                    EncodedCountRecord {
                        first: first_value,
                        second: second_value,
                        hash,
                        third: third_code,
                    },
                    EncodedCountRecord::ALL,
                );
            }
            let after = partitions.iter().map(EncodedCountPartition::footprint).sum::<usize>();
            memory.grow(width_of(after.saturating_sub(before)))?;
            return Ok(true);
        }
        for (row, &third_code) in codes.iter().enumerate().take(rows.rows) {
            let mut valid = if second.is_none() { EncodedCountRecord::SECOND } else { 0 };
            let first_value = if first.is_null_at(row) {
                0
            } else {
                valid |= EncodedCountRecord::FIRST;
                i64::try_from(first.signed_at(row).ok_or_else(|| {
                    Error::internal("an encoded BIGINT key has no signed representation")
                })?)
                .map_err(|_| Error::internal("an encoded BIGINT key is out of range"))?
            };
            let second_value = if let Some(second) = second {
                if second.is_null_at(row) {
                    0
                } else {
                    valid |= EncodedCountRecord::SECOND;
                    i64::try_from(second.signed_at(row).ok_or_else(|| {
                        Error::internal("an encoded BIGINT key has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("an encoded BIGINT key is out of range"))?
                }
            } else {
                0
            };
            let third_value = if third.is_null_at(row) {
                0
            } else {
                valid |= EncodedCountRecord::THIRD;
                if third_code as usize >= dictionary.len() {
                    return Err(Error::internal("an encoded string code is out of range"));
                }
                third_code
            };
            let first_word =
                if valid & EncodedCountRecord::FIRST != 0 { first_value as u64 } else { NOTHING };
            let second_word =
                if valid & EncodedCountRecord::SECOND != 0 { second_value as u64 } else { NOTHING };
            let third_word = if valid & EncodedCountRecord::THIRD != 0 {
                u64::from(third_value)
            } else {
                NOTHING
            };
            let wide = spread(mix(mix(mix(0, first_word), second_word), third_word));
            let hash = (wide ^ (wide >> 32)) as u32;
            partitions[(hash >> shift) as usize].push(
                EncodedCountRecord {
                    first: first_value,
                    second: second_value,
                    hash,
                    third: third_value,
                },
                valid,
            );
        }
        let after = partitions.iter().map(EncodedCountPartition::footprint).sum::<usize>();
        memory.grow(width_of(after.saturating_sub(before)))?;
        Ok(true)
    }

    /// Every value of one chunk into the radix partition its hash picks.
    ///
    /// There are two loops here and they do the same thing. The second one asks the vector for a row
    /// at a time, which costs a match on the layout, a widening to 128 bits and a checked narrowing
    /// back, on every row of the table. The first one reads the run of words where it lies and costs
    /// none of that, and it is the shape a `BIGINT` column of scattered identifiers actually arrives
    /// in, because a range that wide is not worth bit packing. This is the same lift #237 did for the
    /// group hash and #539 did for the key comparison, arriving at the third loop that had it.
    fn buffer_bigint_distinct(
        &self,
        rows: &Rows,
        partitions: &mut [BigIntDistinctPartition],
        memory: &mut Reservation,
    ) -> Result<()> {
        self.bigint_distinct.get_or_init(|| BigIntDistinctExchange {
            partitions: (0..RADIX_PARTITIONS)
                .map(|_| Mutex::new(BigIntDistinctRuns::default()))
                .collect(),
            held: Mutex::new(Vec::new()),
        });
        let Some(column) = rows.arguments.first().and_then(|arguments| arguments.first()) else {
            return Err(Error::internal("a BIGINT distinct exchange received no argument"));
        };
        let before = partitions.iter().map(BigIntDistinctPartition::footprint).sum::<usize>();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        let flat = match column.data() {
            Some(Data::Int64(values)) if !column.validity().has_nulls(rows.rows) => {
                values.get(..rows.rows)
            }
            _ => None,
        };
        if let Some(values) = flat {
            for &value in values {
                scatter_bigint(partitions, shift, value);
            }
        } else {
            for row in 0..rows.rows {
                if column.is_null_at(row) {
                    continue;
                }
                let value = i64::try_from(column.signed_at(row).ok_or_else(|| {
                    Error::internal("a distinct BIGINT value has no signed representation")
                })?)
                .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?;
                scatter_bigint(partitions, shift, value);
            }
        }
        let after = partitions.iter().map(BigIntDistinctPartition::footprint).sum::<usize>();
        memory.grow(width_of(after.saturating_sub(before)))
    }

    /// Reads the whole input and builds the hash table, over as many passes as the budget needs.
    ///
    /// One pass is what this used to be and is what almost every query still does: read everything,
    /// put every group in a table, turn the table into rows. What is new is what happens when the
    /// table cannot hold every group, which is #220, and which on the ClickBench file is `GROUP BY
    /// UserID` and its seventeen million of them.
    ///
    /// A pass that runs out of room keeps the groups it already has and writes any row whose key is
    /// not one of them to a file. Nothing already in the table ever goes to the file, so a key is
    /// either finished in this pass or absent from it entirely, and that is the whole of why this
    /// works: the next pass can aggregate the file on its own, knowing nothing about the rows that
    /// came before, because no group is split across the two.
    ///
    /// It is also why no aggregate state is written out. Splitting the input by row rather than by
    /// key would leave a partial state on each side to be merged, and splitting by key means there
    /// is nothing to merge and every aggregate keeps working unchanged. There is a combine now, so
    /// this could be done the other way, but a spill file of states is a serialize per aggregate and
    /// splitting by key costs nothing, so it stays as it is.
    ///
    /// Each pass gives its table and the rows it made back before the next one starts, so what is
    /// carried between passes is the finished chunks and nothing else. A query whose answer on its
    /// own fills the budget still runs out, which is correct: there is no way to hold seventeen
    /// million rows in room that does not hold them.
    /// One more pass, over the file the pass before it left behind.
    ///
    /// The file is read back through the same fold the pushed chunks went through, so there is one
    /// row loop, one table and one set of charges however many passes a query takes. What comes back
    /// is the next file, or `None` when this pass finished everything that was left.
    fn again(
        &self,
        file: &mut Spill,
        carried: Option<Building>,
        chunks: &mut Vec<Chunk>,
        held: &mut Reservation,
    ) -> Result<Option<Spill>> {
        let mut spilled = Spilled::new(file.read()?, self.spilled_types());
        let mut local = self.start();
        if let Some(error) = local.failure.take() {
            return Err(error);
        }
        if let Some(carried) = carried {
            self.merge(carried, &mut local)?;
        }
        while let Some(rows) = spilled.next(self)? {
            let timing = stage::Timing::start(Stage::Fold);
            let folded = self.fold(&rows, &mut local, None);
            timing.stop(0);
            folded?;
        }
        self.finish(local, chunks, held)
    }

    /// What one instance starts a pass with.
    ///
    /// An ungrouped aggregate has its one group here, which is what makes `SELECT count(*)` over an
    /// empty input answer zero rather than nothing. Building an accumulator can fail, on an
    /// aggregate name nothing implements, and [`Sink::local`] has nowhere to put an error, so the
    /// failure is carried in the instance and reported by the first call that can report it.
    fn start(&self) -> Building {
        let calls = self.calls.len();
        let mut local = Building {
            // The keys and the rows made out of them, given back when this pass ends, because by
            // then they are in the chunks.
            scratch: self.memory.reservation(),
            // The three containers and the sets a `DISTINCT` fills, which are gone before the
            // chunks are built rather than after. Their own reservation so that their charge can go
            // when they do, which is what leaves room for the chunks. A key is not in here, because
            // a key is moved into the rows and outlives all of it. Per #272.
            containers: self.memory.reservation(),
            charged: 0,
            // What the keys the table has taken a copy of own away from themselves, charged against
            // the scratch rather than against the containers because those strings move into the
            // rows and outlive the table. `charged` and this one are the same arrangement over two
            // reservations.
            charged_keys: 0,
            table: Table::new(
                &self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect::<Vec<_>>(),
            ),
            states: Vec::new(),
            counts: Vec::new(),
            compact: Vec::new(),
            overflow: HashMap::new(),
            seen: Vec::new(),
            groups: 0,
            // One row of arguments per call, filled again for each input row and kept between rows
            // so that the buffers behind them are asked for once and not once per row. Only a row
            // that turns out to be new to a `DISTINCT` is copied out of one.
            given: vec![Key(Vec::new()); calls],
            // One hash per row of the chunk in hand, built a column at a time before the row loop
            // starts. Kept between chunks for the reason the buffers above are.
            hashes: Vec::new(),
            // One slot per row of the chunk in hand, which is what the probe produces and what the
            // scatter consumes, and the same slots with a call's `FILTER` folded into them.
            slots: Vec::new(),
            walk: Walk::default(),
            // The direct map over combinations of codes, and the dictionaries it belongs to. Both
            // empty until a chunk arrives that it can answer, and kept across the chunks of a row
            // group, which is the whole point of holding them here.
            coded_on: Vec::new(),
            coded_map: Vec::new(),
            missing: Vec::new(),
            same: Vec::new(),
            leaders: Vec::new(),
            leader_slots: Vec::new(),
            kept: Vec::new(),
            affine_rows: vec![0; calls],
            // The file the rows that do not fit go to, made the first time the budget says the
            // table has to stop growing and `None` for as long as it does not. One row of it, kept
            // between rows so that writing does not go to the allocator per row.
            over: None,
            away: Vec::new(),
            failure: None,
        };
        if self.alone {
            local.groups = 1;
            if let Err(error) = self.fresh(&mut local.states, &mut local.counts, &mut local.compact)
            {
                local.failure = Some(error);
            }
            if self.sets {
                self.fresh_seen(&mut local.seen);
            }
        }
        local
    }

    /// One chunk of rows folded into the table.
    fn fold(
        &self,
        seen_rows: &Rows,
        local: &mut Building,
        prehashed: Option<&[u64]>,
    ) -> Result<()> {
        // Field by field, because the `DISTINCT` path below holds four of them at once and they
        // have to be disjoint borrows.
        let Building {
            scratch,
            containers,
            charged,
            charged_keys,
            table,
            states,
            counts,
            compact,
            overflow,
            seen,
            groups,
            given,
            hashes,
            slots,
            walk,
            coded_on,
            coded_map,
            missing,
            same,
            leaders,
            leader_slots,
            kept,
            affine_rows,
            over,
            away,
            failure: _,
        } = local;
        let calls = self.calls.len();
        let alone = self.alone;
        let Rows { keys, arguments, filters, rows: length } = seen_rows;
        let mut aside = 0;
        for at in 0..calls {
            if self.calls[at].affine.is_some() {
                continue;
            }
            if self.by_vector[at] {
                states[at].update_run(&arguments[at], *length)?;
                if self.calls.iter().any(|call| call.affine.is_some_and(|(source, _)| source == at))
                {
                    affine_rows[at] +=
                        i64::try_from(arguments[at][0].validity().count_valid(*length))
                            .map_err(|_| Error::out_of_range("too many rows in an aggregate"))?;
                }
            }
        }
        if alone && self.every {
            return Ok(());
        }
        // The column at a time half of #237. One pass over each key column turns the whole chunk
        // into one hash per row, with the type of the column matched on once rather than once per
        // value, and the row loop below is then a probe with the hash already in hand.
        // The probe, and nothing else. What comes out of it is one slot per row, which is what the
        // scatter below needs and what the row loop used to consume as it went.
        slots.clear();
        slots.resize(*length, if alone { 0 } else { NOWHERE });
        // The direct map first, because a chunk it answers is a chunk that is never hashed. The
        // whole key of q1 is two dictionary codes with six combinations between them, so the map is
        // six slots long and every row after the first six is a multiply add and a load. See
        // [`Coded`](crate::table::Coded) for why that is the shape a Parquet scan hands over.
        //
        // Refused while there is a spill file, because a row that does not fit goes out whole and
        // the map has nothing to say about where it went.
        let direct = if alone || over.is_some() {
            coded_on.clear();
            None
        } else {
            crate::table::coded(keys, *length)
        };
        if let Some(codes) = &direct {
            if !codes.same_as(coded_on) {
                codes.hold(coded_on);
                coded_map.clear();
                coded_map.resize(codes.combos(), NOWHERE);
            }
            missing.clear();
            for (row, slot) in slots.iter_mut().enumerate() {
                let found = coded_map[codes.at(row)];
                if found == NOWHERE {
                    missing.push(row);
                } else {
                    *slot = found;
                }
            }
        } else {
            coded_on.clear();
        }
        // Hashed unless the map answered the whole chunk, which is the ordinary case once the first
        // rows of a row group have been through.
        if !alone && direct.as_ref().is_none_or(|_| !missing.is_empty()) {
            match prehashed {
                Some(prehashed) => {
                    hashes.clear();
                    hashes.extend_from_slice(prehashed);
                }
                None => crate::table::hash(keys, *length, hashes, crate::table::Across::OneInput),
            }
        }
        // A batch at a time, because a probe of a table larger than the cache is three dependent
        // misses on a row and the only way to overlap them is to have several rows in flight at once.
        // What comes back is every row whose key is already a group, filled in, and the rest in row
        // order. Those go one at a time: a key that is not in the table either starts a group or goes
        // out to the spill file, and both of them change what the row after would have found.
        // The rows the map had nothing for, which are the first row of each combination and no
        // others. They go through the probe and the insert every row used to go through, and what
        // comes back is written into the map so that the rest of the row group skips both.
        if let Some(codes) = &direct {
            for &row in missing.iter() {
                let index = codes.at(row);
                // Two rows of one chunk can be the first two of one combination, and the first of
                // them filled the map on its way past.
                if coded_map[index] != NOWHERE {
                    slots[row] = coded_map[index];
                    continue;
                }
                let bucket = match table.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => {
                        slots[row] = slot;
                        coded_map[index] = slot;
                        continue;
                    }
                    Probe::Vacant(bucket) => bucket,
                };
                if self.max_groups.is_some_and(|limit| table.len() >= limit) {
                    continue;
                }
                slots[row] = table.insert(bucket, hashes[row], keys, row)?;
                coded_map[index] = slots[row];
                *groups = table.len();
                self.fresh(states, counts, compact)?;
                if self.sets {
                    self.fresh_seen(seen);
                }
            }
        }
        // Whether the chunk arrives in runs of one key, and if it does, which rows start one.
        //
        // A column the rows happen to be sorted on asks the table for the same group over and over.
        // Three quarters of lineitem's rows carry the order key of the row before them, so a
        // `GROUP BY l_orderkey` over it walks the buckets four times for every answer it needs
        // once. The run pass reads the key columns where they already are, in one sequential pass
        // each, and what it marks is probed once for the whole run.
        //
        // Refused while there is a spill file, because a row whose leader went out to the file
        // would have to go out too and it is not the leader that carries its columns, and refused
        // under a group limit, because the row the limit turns away leaves its run nothing to copy.
        // The direct map already answered its chunk without probing, so there is nothing to save
        // there either.
        //
        // Half of the chunk, because what the run path saves is a probe for every row it marks and
        // what it costs is one sequential pass per key column plus the list, so a chunk where every
        // other row repeats is already well ahead and one where fewer do is not worth the risk of
        // being behind. A chunk that cannot reach it is dropped inside the pass.
        let runs = if direct.is_none() && !alone && over.is_none() && self.max_groups.is_none() {
            crate::table::repeats(keys, *length, length.div_ceil(2), same)
        } else {
            same.clear();
            0
        };
        let by_run = runs > 0;
        if by_run {
            leaders.clear();
            leaders.extend((0..*length).filter(|&row| !same[row]));
        }
        let mut from = 0;
        while by_run && from < leaders.len() {
            let upto = (from + crate::table::BATCH).min(leaders.len());
            let batch = &leaders[from..upto];
            from = upto;
            leader_slots.clear();
            leader_slots.resize(batch.len(), NOWHERE);
            table.probe_these(hashes, keys, batch, leader_slots, walk);
            // By place in the batch rather than by row, which is how a list is probed and answered.
            for &place in walk.pending() {
                let row = batch[place];
                let bucket = match table.probe(hashes[row], keys, row) {
                    Probe::Found(slot) => {
                        leader_slots[place] = slot;
                        continue;
                    }
                    Probe::Vacant(bucket) => bucket,
                };
                leader_slots[place] = table.insert(bucket, hashes[row], keys, row)?;
                *groups = table.len();
                self.fresh(states, counts, compact)?;
                if self.sets {
                    self.fresh_seen(seen);
                }
            }
            for (place, &row) in batch.iter().enumerate() {
                slots[row] = leader_slots[place];
            }
        }
        if by_run {
            // Every row that is not a leader holds the key of the row before it, so it is in the
            // group that row is in. Forwards, because the row before is either a leader that the
            // loop above filled or a member this loop filled on the way past.
            for row in 1..*length {
                if same[row] {
                    slots[row] = slots[row - 1];
                }
            }
        }
        let mut from = 0;
        while !by_run && direct.is_none() && !alone && from < *length {
            let upto = (from + crate::table::BATCH).min(*length);
            table.probe_run(hashes, keys, from, upto, slots, walk);
            from = upto;
            for &row in walk.pending() {
                let bucket = match table.probe(hashes[row], keys, row) {
                    // An earlier row of the same batch started this group.
                    Probe::Found(slot) => {
                        slots[row] = slot;
                        continue;
                    }
                    Probe::Vacant(bucket) => bucket,
                };
                if self.max_groups.is_some_and(|limit| table.len() >= limit) {
                    continue;
                }
                if let Some(file) = over.as_mut() {
                    // The table is as large as the budget will let it be and this key is not in it,
                    // so the row goes out whole. Every later row with this key goes out too, because
                    // the key is never inserted here, and that is what lets the next pass finish the
                    // group without knowing anything about this one.
                    put_away(file, seen_rows, row, away)?;
                    continue;
                }
                // A group costs the copy of its key that the table takes, and its own accumulators
                // and distinct sets in the two vectors beside it. What all of those took to have room
                // for it is charged below and once per chunk, because it is a property of the
                // containers rather than of this group, and what the key owns away from itself the
                // table adds up as it goes and is charged the same way.
                slots[row] = table.insert(bucket, hashes[row], keys, row)?;
                *groups = table.len();
                self.fresh(states, counts, compact)?;
                if self.sets {
                    self.fresh_seen(seen);
                }
            }
        }
        if self.compact_numeric {
            let sum = arguments[1].first().expect("SUM has one argument");
            let mean = arguments[2].first().expect("AVG has one argument");
            let sum_flat = flat_smallint(sum);
            let mean_flat = flat_smallint(mean);
            let read =
                |column: &Vector, values: Option<&[i16]>, row: usize| -> Result<Option<i16>> {
                    match values {
                        Some(values) if column.validity().is_valid(row) => Ok(Some(values[row])),
                        Some(_) => Ok(None),
                        None => match column.value_at(row) {
                            Value::SmallInt(value) => Ok(Some(value)),
                            Value::Null => Ok(None),
                            value => Err(Error::internal(format!(
                                "a compact SMALLINT aggregate received {value:?}"
                            ))),
                        },
                    }
                };
            // row at a time: each group needs its own two totals. Flat SMALLINT columns are read
            // directly; the other vector forms use the general accessor in `read` above.
            for (row, &slot) in slots.iter().enumerate() {
                if slot == NOWHERE {
                    continue;
                }
                let state = &mut compact[slot];
                state.add(
                    slot,
                    read(sum, sum_flat, row)?,
                    read(mean, mean_flat, row)?,
                    overflow,
                )?;
            }
        }
        if self.count_only {
            for &slot in slots.iter() {
                if slot != NOWHERE {
                    counts[slot] += 1;
                }
            }
        }
        // The aggregate half of #61. Every call that is not `DISTINCT` folds the whole chunk in one
        // pass, with the aggregate and the layout of its argument matched on once for the chunk
        // rather than once per row, and with no `Value` built at all on the paths the kernel covers.
        for (at, call) in self.calls.iter().enumerate() {
            if self.count_only || self.compact_numeric {
                break;
            }
            if self.by_vector[at] || call.affine.is_some() {
                continue;
            }
            if call.distinct {
                aside += self.distinct(states, seen, seen_rows, slots, at, given)?;
                continue;
            }
            let picked = match &filters[at] {
                None => &*slots,
                Some(flags) => {
                    // A row the filter dropped belongs to nothing, which is the same thing the
                    // scatter already understands a spilled row to be, so the filter goes into the
                    // slots rather than into the loop that reads them.
                    kept.clear();
                    kept.extend(slots.iter().enumerate().map(|(row, &slot)| {
                        if slot != NOWHERE && is_true(&flags.value_at(row)) {
                            slot
                        } else {
                            NOWHERE
                        }
                    }));
                    &*kept
                }
            };
            update_scattered(states, picked, calls, at, arguments[at].first(), *length)?;
        }
        rows::capacity(table.owned(), charged_keys, scratch)?;
        containers.grow(aside)?;
        let now = tables(table, states, counts, compact, overflow, seen);
        rows::capacity(now, charged, containers)?;
        // Asked after the chunk has been folded in and not before, so that a pass always takes at
        // least one chunk of groups whatever the budget says. That is what makes the loop in
        // `combine` finish: a pass that could spill from its first row would spill every row and
        // hand back a file the same size as what it was given.
        match over.as_ref() {
            None if !alone && crowded(&self.memory) => {
                *over = Some(Spill::new("aggregate", self.spilled_types())?);
            }
            Some(file) => hopeless(file, *groups)?,
            None => {}
        }
        Ok(())
    }

    /// The end of a pass: the table becomes chunks and whatever did not fit is handed back.
    ///
    /// The chunks the finished groups make are appended to `chunks` and charged against `held`,
    /// which the operator holds for as long as it holds them. What comes back is the file the rows
    /// that did not fit went to, and `None` when every row fit, which is the ordinary case and the
    /// only case before #220.
    ///
    /// The rows are turned into chunks here rather than once at the end for the memory rather than
    /// for the tidiness. A row and the chunk built from it are two copies of the same values, and
    /// keeping the rows of every pass until the last pass ended would hold both copies of the whole
    /// answer at once. Ending the pass with the chunks alone means the second copy is only ever of
    /// what one pass finished.
    fn finish(
        &self,
        local: Building,
        chunks: &mut Vec<Chunk>,
        held: &mut Reservation,
    ) -> Result<Option<Spill>> {
        let timing = stage::Timing::start(Stage::Emit);
        let finished = self.finishing(local, chunks, held);
        timing.stop(0);
        finished
    }

    /// [`Aggregate::finish`] with the clock taken off it, so that the clock wraps all of it.
    fn finishing(
        &self,
        local: Building,
        chunks: &mut Vec<Chunk>,
        held: &mut Reservation,
    ) -> Result<Option<Spill>> {
        let Building {
            mut scratch,
            mut containers,
            table,
            mut states,
            counts,
            compact,
            overflow,
            seen,
            groups,
            affine_rows,
            over,
            ..
        } = local;
        let calls = self.calls.len();
        // The distinct sets are finished with and the table and the accumulators are not, so the
        // charge for the sets goes here rather than after the chunks are built, which is part of
        // the room the chunks are built in.
        drop(seen);
        let alive = table.footprint()
            + width_of(states.capacity() * size_of::<Accumulator>())
            + width_of(counts.capacity() * size_of::<i64>())
            + width_of(compact.capacity() * size_of::<CompactNumeric>())
            + overflow_footprint(&overflow);
        containers.shrink(containers.bytes().saturating_sub(alive));
        // The answer is built straight out of the table, a chunk of groups at a time.
        //
        // This used to go through `Vec<Vec<Value>>`, which meant every group was a block from the
        // allocator, the keys were transposed out of the table into rows and then transposed back
        // into columns to make a chunk, and each key value was cloned on the way. On the ClickBench
        // queries that group on something close to one group per row that was most of what the
        // operator did, and none of it was work: the table already holds the keys one column at a
        // time, which is the shape a chunk wants.
        //
        // A chunk at a time and not all of it, so what is held at once is the answer plus one
        // chunk. The ungrouped case falls out of the same loop with no key columns and one group.
        let types = self.schema.types();
        let width = self.groups.len();
        let count = |slot: usize, call: usize| {
            if self.count_only {
                return counts[slot];
            }
            if self.compact_numeric {
                return compact[slot].count();
            }
            states[slot * calls + call].counted().expect("a selected COUNT call has a COUNT state")
        };
        let selected = match (self.top_counts, self.having_count) {
            (Some((bound, ranks)), _) => {
                let mut best = Vec::with_capacity(bound.min(groups));
                for slot in 0..groups {
                    let at = best.partition_point(|&kept| count(kept, ranks) >= count(slot, ranks));
                    if at < bound {
                        best.insert(at, slot);
                        best.truncate(bound);
                    }
                }
                // The downstream TopN settles equal keys by arrival. Preserve the order this
                // partition would have emitted without the reduction.
                best.sort_unstable();
                Some(best)
            }
            (_, Some((call, minimum))) => {
                let mut kept = Vec::new();
                for slot in 0..groups {
                    if count(slot, call) >= minimum {
                        kept.push(slot);
                    }
                }
                Some(kept)
            }
            _ => None,
        };
        let output_groups = selected.as_ref().map_or(groups, Vec::len);
        // A min or a max over a dictionary column is holding a code rather than a string, and the
        // strings come out of the payload in one ordered sweep here rather than one point read per
        // group down in the loop. Only the groups that are going to be emitted, since the selection
        // above has already thrown the rest away.
        if !self.count_only && !self.compact_numeric {
            settle_extremes(&mut states, selected.as_deref(), groups, calls)?;
        }
        // The one buffer the results of a call go through on their way into a vector, kept between
        // chunks and charged once.
        scratch.grow(width_of(VECTOR_SIZE.min(output_groups) * size_of::<Value>()))?;
        let mut results: Vec<Value> = Vec::new();
        // row at a time: the outer loop steps a chunk at a time and the key columns are copied a
        // column at a time out of the table, so the only thing left here that is per group is asking
        // each accumulator for its result, which is 2g (#61).
        for start in (0..output_groups).step_by(VECTOR_SIZE) {
            let end = (start + VECTOR_SIZE).min(output_groups);
            let slots = selected.as_ref().map(|slots| &slots[start..end]);
            let mut columns = Vec::with_capacity(width + calls);
            let mut key = 0;
            for (at, ty) in types.iter().take(width).enumerate() {
                if let Some(value) = &self.constants[at] {
                    columns.push(Vector::constant(ty.clone(), value.clone(), end - start));
                } else {
                    columns.push(match slots {
                        Some(slots) => table.column_slots(key, ty, slots)?,
                        None => table.column(key, ty, start..end)?,
                    });
                    key += 1;
                }
            }
            for (at, ty) in types.iter().skip(width).enumerate() {
                // What a result owns away from itself is not knowable until it has been asked for,
                // so that part is charged as it arrives and given back once it is in the vector.
                let mut taken = 0;
                results.clear();
                for index in start..end {
                    let slot = slots.map_or(index, |slots| slots[index - start]);
                    let value = if self.count_only {
                        Ok(Value::BigInt(counts[slot]))
                    } else if self.compact_numeric {
                        let state = &compact[slot];
                        let (sum, mean) = state.totals(slot, &overflow);
                        match at {
                            0 => Ok(Value::BigInt(state.count())),
                            1 => Accumulator::exact_sum(
                                sum,
                                state.sum_seen(),
                                &self.calls[1].returns,
                            )
                            .finish(),
                            2 => Accumulator::exact_avg(
                                mean,
                                state.mean_count,
                                &self.calls[2].returns,
                            )
                            .finish(),
                            _ => unreachable!("compact numeric has three calls"),
                        }
                    } else {
                        match self.calls[at].affine {
                            Some((source, offset)) => states[slot * calls + source]
                                .finish_offset(offset, affine_rows[source]),
                            None => states[slot * calls + at].finish(),
                        }
                    }?;
                    taken += rows::owned(&value);
                    results.push(value);
                }
                scratch.grow(taken)?;
                columns.push(Vector::from_values(ty.clone(), &results)?);
                scratch.shrink(taken);
            }
            let chunk = Chunk::with_rows(columns, end - start)?;
            held.grow(width_of(chunk.footprint()))?;
            chunks.push(chunk);
        }
        drop(results);
        drop(states);
        drop(counts);
        drop(compact);
        drop(table);
        containers.release();
        match over {
            // A pass that put nothing in its table and still wrote rows out would hand back what it
            // was given and the next pass would do the same. It cannot happen, because the spill
            // only opens after a chunk has gone in, and it is checked rather than assumed because
            // the alternative to an error here is a loop that never ends.
            Some(file) if groups == 0 && file.rows() > 0 => Err(Error::out_of_memory(format!(
                "the memory limit does not leave room for a single group of this aggregate, \
                 {} rows and {} bytes went to a spill file and none of them could be finished",
                file.rows(),
                file.bytes()
            ))),
            Some(file) if file.rows() > 0 => Ok(Some(file)),
            _ => Ok(None),
        }
    }

    /// Folds one instance's table into another's, so that an aggregate can run on more than one
    /// thread.
    ///
    /// The key half is the probe the fold already does. Every group the incoming table holds is
    /// looked up in the kept one, and the hash it is looked up by is the hash the incoming table
    /// stored when the group went in, because both tables came from this operator and so hashed the
    /// same way. A group that is already there has its accumulators folded in by
    /// [`Accumulator::combine`]. A group that is not is inserted, which copies its key across, and
    /// gets a fresh set of accumulators to fold into.
    ///
    /// The keys come out a chunk at a time and a column at a time, which is the shape the table
    /// already holds them in and the shape the probe wants, so the only thing per group here is the
    /// probe itself and the run of accumulator merges after it.
    ///
    /// # The `DISTINCT` half
    ///
    /// A `DISTINCT` call keeps a set of the values it has already accepted, one set per group per
    /// call, so merging two of those groups means putting the two sets together. That is done by
    /// offering the incoming set's values to the kept one and folding in only the ones it did not
    /// already have, which is the same thing the fold does with a row and gives the same answer for
    /// every aggregate rather than only for counting. The incoming set is moved rather than read,
    /// so a value that is new is handed over and a value that is not is dropped, and neither is
    /// copied.
    ///
    /// It costs a pass over the smaller table's sets, which is proportional to the distinct values
    /// in them rather than to the rows that were read, and it is what lets a query with a
    /// `COUNT(DISTINCT ...)` in it run its scan on more than one thread at all. That mattered:
    /// nine of the 43 ClickBench queries have one, and until this they held their whole pipeline on
    /// one thread and were 43 percent of the time the suite took at ten million rows. See #509.
    ///
    /// # What is refused
    ///
    /// An instance that spilled, because spilling rests on a key being either finished in this pass
    /// or absent from it entirely, and that holds within one instance and not across two. A key can
    /// be in one instance's table and in another instance's file at the same time, and the later
    /// pass over that file would then finish a group that is already finished. Radix partitioning
    /// is what fixes this, because a partition is finished by one thread and the invariant comes
    /// back, and that is the next item on the roadmap rather than this one.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::NotImplemented`] for that one. Whatever the probe, the insert or
    /// an accumulator merge reports otherwise.
    fn merge(&self, from: Building, into: &mut Building) -> Result<()> {
        let timing = stage::Timing::start(Stage::Merge);
        let merged = self.merging(from, into);
        timing.stop(0);
        merged
    }

    /// [`Aggregate::merge`] with the clock taken off it, so that the clock wraps all of it.
    fn merging(&self, from: Building, into: &mut Building) -> Result<()> {
        if from.over.is_some() || into.over.is_some() {
            return Err(Error::internal(
                "two tables of an aggregate were merged with a spill file between them, where a \
                 key can be in one table and in the other's file at once, which the callers avoid \
                 by partitioning instead",
            ));
        }
        let Building {
            scratch,
            containers,
            table: source,
            states: taken,
            counts: tallies,
            compact: packed,
            overflow: wide,
            seen: mut watched,
            groups: found,
            affine_rows: counted,
            ..
        } = from;
        let calls = self.calls.len();
        for (at, rows) in counted.iter().enumerate() {
            into.affine_rows[at] += rows;
        }
        let distinct: Vec<bool> = self.calls.iter().map(|call| call.distinct).collect();
        let mut coming = Folding {
            count_only: self.count_only,
            calls,
            distinct: &distinct,
            taken: &taken,
            tallies: &tallies,
            compact: &packed,
            overflow: &wide,
            watched: &mut watched,
        };
        let mut aside = 0;
        if self.alone {
            // One slot each and no key at all, so there is nothing to look up and the merge is the
            // states on their own.
            aside += merge_slot(&mut coming, 0, 0, into)?;
        } else {
            let types: Vec<LogicalType> =
                self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect();
            let mut run: Vec<usize> = Vec::with_capacity(VECTOR_SIZE);
            for start in (0..found).step_by(VECTOR_SIZE) {
                let end = (start + VECTOR_SIZE).min(found);
                let mut keys = Vec::with_capacity(types.len());
                for (at, ty) in types.iter().enumerate() {
                    keys.push(source.column(at, ty, start..end)?);
                }
                run.clear();
                run.extend(start..end);
                aside += self.fold_slots(&mut coming, &source, &keys, start, &run, into)?;
            }
        }
        // Before the incoming instance's charge goes back, because the values that moved between
        // the two sets were held by both for as long as the move took.
        into.containers.grow(aside)?;
        drop(watched);
        drop(taken);
        drop(tallies);
        drop(source);
        // The incoming instance is spent, so its charge goes back, and what the kept one grew to
        // taking is charged in its place. Both in that order, because the merge held the two at once
        // and the peak really was the sum.
        drop(scratch);
        drop(containers);
        rows::capacity(into.table.owned(), &mut into.charged_keys, &mut into.scratch)?;
        let now = tables(
            &into.table,
            &into.states,
            &into.counts,
            &into.compact,
            &into.overflow,
            &into.seen,
        );
        rows::capacity(now, &mut into.charged, &mut into.containers)
    }

    /// Some of one table's groups folded into another, given the keys of the run they are in.
    ///
    /// `slots` are slots of `source`, all of them inside `start .. start + keys.len()`, because
    /// `keys` is the columns of that run and a probe wants a row of it rather than a slot. A merge
    /// hands this the whole run. A scatter hands it only the slots belonging to one partition.
    ///
    /// What comes back is what the values that moved own away from themselves, which the caller
    /// charges once rather than once per group.
    fn fold_slots(
        &self,
        coming: &mut Folding<'_>,
        source: &Table,
        keys: &[Vector],
        start: usize,
        slots: &[usize],
        into: &mut Building,
    ) -> Result<u64> {
        let mut aside = 0;
        // row at a time: the keys came out a column at a time above, so what is left per group is
        // one probe and the accumulators behind it, which is 2g (#61).
        for &slot in slots {
            let row = slot - start;
            let hash = source.hash_of(slot);
            let target = match into.table.probe(hash, keys, row) {
                Probe::Found(target) => target,
                Probe::Vacant(bucket) => {
                    // The same cap the fold applies, for the same reason: a limit above an
                    // unordered group by only ever looks at so many groups, and one that is dropped
                    // here would have been dropped there.
                    if self.max_groups.is_some_and(|limit| into.table.len() >= limit) {
                        continue;
                    }
                    let target = into.table.insert(bucket, hash, keys, row)?;
                    into.groups = into.table.len();
                    self.fresh(&mut into.states, &mut into.counts, &mut into.compact)?;
                    if self.sets {
                        self.fresh_seen(&mut into.seen);
                    }
                    target
                }
            };
            aside += merge_slot(coming, slot, target, into)?;
        }
        Ok(aside)
    }

    /// One table's groups broken up into the shared partitions, group by group.
    ///
    /// The in memory half of [`Aggregate::hand_over`], which is the only caller, because what the
    /// table spilled has to be put back a row at a time and this puts groups. Each group ends up in
    /// the one partition its hash picks, which is what stops it living in two places at once and
    /// coming out of `finalize` twice.
    ///
    /// The partitions are taken from a rotating start for the reason [`Aggregate::spread`] takes
    /// them that way, and each is held only for the groups that belong to it.
    ///
    /// A partition that has started spilling takes only the groups whose keys it already holds. The
    /// rest go into [`Partition::carried`], because the file the partition is filling holds rows for
    /// every key it does not hold, and a group put in the table here would then be finished once out
    /// of the table and once out of the file.
    fn scatter(&self, from: Building, spin: usize) -> Result<()> {
        let timing = stage::Timing::start(Stage::Scatter);
        let scattered = self.scattering(from, spin);
        timing.stop(0);
        scattered
    }

    /// [`Aggregate::scatter`] with the clock taken off it, so that the clock wraps all of it.
    fn scattering(&self, from: Building, spin: usize) -> Result<()> {
        debug_assert!(from.over.is_none(), "a spill file is drained by hand_over, not scattered");
        let Building {
            scratch,
            containers,
            table: source,
            states: taken,
            counts: tallies,
            compact: packed,
            overflow: wide,
            seen: mut watched,
            groups: found,
            ..
        } = from;
        let calls = self.calls.len();
        let distinct: Vec<bool> = self.calls.iter().map(|call| call.distinct).collect();
        let mut coming = Folding {
            count_only: self.count_only,
            calls,
            distinct: &distinct,
            taken: &taken,
            tallies: &tallies,
            compact: &packed,
            overflow: &wide,
            watched: &mut watched,
        };
        let types: Vec<LogicalType> =
            self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); RADIX_PARTITIONS];
        // The two halves a spilling partition splits its share into, kept out here so that the rare
        // path asks the allocator once rather than once per chunk of groups.
        let mut here: Vec<usize> = Vec::new();
        let mut late: Vec<usize> = Vec::new();
        // `affine_rows` is not carried over the way `merge` carries it, because it is only ever
        // filled on the vector at a time path, that path is only taken when the aggregate is
        // ungrouped, and an ungrouped aggregate never gets here. A grouped table's counts are zero.
        for start in (0..found).step_by(VECTOR_SIZE) {
            let end = (start + VECTOR_SIZE).min(found);
            let mut keys = Vec::with_capacity(types.len());
            for (at, ty) in types.iter().enumerate() {
                keys.push(source.column(at, ty, start..end)?);
            }
            for bucket in &mut buckets {
                bucket.clear();
            }
            for slot in start..end {
                buckets[(source.hash_of(slot) >> shift) as usize].push(slot);
            }
            for step in 0..RADIX_PARTITIONS {
                let at = (step + spin) % RADIX_PARTITIONS;
                if buckets[at].is_empty() {
                    continue;
                }
                let mut held = self.merged[at].lock().map_err(poisoned)?;
                let Partition { table, carried, .. } = &mut *held;
                let into = table.get_or_insert_with(|| self.start());
                if into.over.is_none() {
                    let grown =
                        self.fold_slots(&mut coming, &source, &keys, start, &buckets[at], into)?;
                    charge(into, grown)?;
                    continue;
                }
                // The partition has started spilling, so a key it does not already hold is one whose
                // rows went to its file, and putting the group in the table here would mean the
                // table answers for it and the file answers for it again. Those groups go aside.
                // Which ones they are is asked once, out here, so that the fold below is the same
                // fold every other path runs.
                here.clear();
                late.clear();
                for &slot in &buckets[at] {
                    match into.table.probe(source.hash_of(slot), &keys, slot - start) {
                        Probe::Found(_) => here.push(slot),
                        Probe::Vacant(_) => late.push(slot),
                    }
                }
                let grown = self.fold_slots(&mut coming, &source, &keys, start, &here, into)?;
                charge(into, grown)?;
                let waiting = carried.get_or_insert_with(|| self.start());
                let grown = self.fold_slots(&mut coming, &source, &keys, start, &late, waiting)?;
                charge(waiting, grown)?;
            }
        }
        drop(watched);
        drop(taken);
        drop(tallies);
        drop(source);
        drop(scratch);
        drop(containers);
        Ok(())
    }

    /// Agree with the other instances on which groups a pushed down limit keeps.
    ///
    /// Two steps and they have to happen in this order. First this chunk's keys go into the agreed
    /// set, until there are as many as the limit. Then, if the set is full, it goes into this
    /// instance's own table, which from that moment holds the limit's worth of groups and opens no
    /// more.
    ///
    /// The order is what makes it sound. Coming out of the first step either the set is full, and the
    /// second step puts all of it here so this chunk is folded against the agreed groups, or it is not
    /// full, and then every key of this chunk is in it, so a group the fold opens for this chunk is a
    /// group every other instance will keep too. There is no chunk in between the two where an
    /// instance can open a group that nobody else has.
    fn agree(
        &self,
        rows: &Rows,
        limit: usize,
        into: &mut Building,
        installed: &mut bool,
    ) -> Result<()> {
        if !self.settled.load(Ordering::Acquire) {
            self.collect(rows, limit)?;
        }
        if *installed || !self.settled.load(Ordering::Acquire) {
            return Ok(());
        }
        let keys = {
            let held = self.agreed.lock().map_err(poisoned)?;
            let agreed = held
                .as_ref()
                .ok_or_else(|| Error::internal("a limited aggregate settled on nothing"))?;
            let keys = agreed
                .keys
                .as_ref()
                .ok_or_else(|| Error::internal("a limited aggregate settled without keys"))?;
            Arc::clone(keys)
        };
        self.install(&keys, into)?;
        *installed = true;
        Ok(())
    }

    /// This chunk's keys into the agreed set, and the set sealed once it is as large as the limit.
    ///
    /// Under one lock, so this is the serial part of a limited aggregate. It lasts as long as it
    /// takes to see the limit's worth of distinct keys, which for a `LIMIT 10` over a million rows is
    /// the first chunk and nothing after it.
    fn collect(&self, rows: &Rows, limit: usize) -> Result<()> {
        let mut held = self.agreed.lock().map_err(poisoned)?;
        let agreed = match held.as_mut() {
            Some(agreed) => agreed,
            None => {
                let types: Vec<LogicalType> =
                    rows.keys.iter().map(|column| column.logical_type().clone()).collect();
                held.insert(Agreed { table: Table::new(&types), hashes: Vec::new(), keys: None })
            }
        };
        if agreed.keys.is_some() {
            return Ok(());
        }
        crate::table::hash(
            &rows.keys,
            rows.rows,
            &mut agreed.hashes,
            crate::table::Across::OneInput,
        );
        // row at a time: a key that is not in the set starts a group in it, which changes what the
        // key after would have found, and the set is at most the limit long so there is no run to
        // batch.
        for row in 0..rows.rows {
            if agreed.table.len() >= limit {
                break;
            }
            let hash = agreed.hashes[row];
            if let Probe::Vacant(bucket) = agreed.table.probe(hash, &rows.keys, row) {
                agreed.table.insert(bucket, hash, &rows.keys, row)?;
            }
        }
        if agreed.table.len() < limit {
            return Ok(());
        }
        let mut keys = Vec::with_capacity(rows.keys.len());
        for (at, column) in rows.keys.iter().enumerate() {
            keys.push(agreed.table.column(at, column.logical_type(), 0..agreed.table.len())?);
        }
        agreed.keys = Some(Arc::new(keys));
        self.settled.store(true, Ordering::Release);
        Ok(())
    }

    /// The agreed keys into one instance's table, as the groups it is allowed to keep.
    ///
    /// A group that is already there stays where it is, because this instance has been counting into
    /// it and its slot is the order it was first seen in.
    fn install(&self, keys: &[Vector], into: &mut Building) -> Result<()> {
        let rows = keys.first().map_or(0, Vector::len);
        let Building { table, states, counts, compact, seen, groups, hashes, .. } = into;
        crate::table::hash(keys, rows, hashes, crate::table::Across::OneInput);
        // row at a time: same as the set above, and there are at most a limit's worth of them.
        for (row, &hash) in hashes.iter().enumerate().take(rows) {
            if let Probe::Vacant(bucket) = table.probe(hash, keys, row) {
                table.insert(bucket, hash, keys, row)?;
                *groups = table.len();
                self.fresh(states, counts, compact)?;
                if self.sets {
                    self.fresh_seen(seen);
                }
            }
        }
        Ok(())
    }

    /// Whether an instance holding this table should hand it to the partitions and stop keeping one.
    ///
    /// Four things have to hold. There has to be more than one instance, because sharing a table
    /// with nobody is all cost. There has to be no pushed down limit, since that path is refused a
    /// second instance anyway and counts groups against a cap that a partition cannot see. The
    /// aggregate has to be grouped, because an ungrouped one has a single slot and no key to hash.
    /// And the table has to be large enough to be worth the split, which is [`PARTITION_FROM`].
    ///
    /// A crowded budget counts as large enough whatever the group count says. An instance that is
    /// about to be told to spill is better off in the partitions, because the shared tables hold
    /// what N instance tables held and the room that frees may be all that was needed. It also keeps
    /// the ordinary case away from the awkward one: a table that spills before it is handed over has
    /// a file covering every partition, and [`Aggregate::hand_over`] has to drain it row by row.
    fn ought_to_partition(&self, table: &Building) -> bool {
        !self.alone
            && self.max_groups.is_none()
            && self.started.load(Ordering::Relaxed) > 1
            && (table.groups >= self.partition_from() || crowded(&self.memory))
    }

    /// How large a table has to be before splitting it is worth doing.
    ///
    /// [`PARTITION_FROM`] ordinarily, and more than that when a count descending TopN above has
    /// pushed its bound down here. That bound is applied when a table is finished, and after a split
    /// there is a table to finish per partition rather than one, so it is applied once per
    /// [`RADIX_PARTITIONS`] and lets through that many times as many rows. It only stops letting
    /// through more than it should once each partition would still hold more groups than the bound,
    /// which is what this asks for.
    ///
    /// ClickBench 39 is the query that showed it. It groups five columns down to 5445 groups under a
    /// bound of 1010, so one table gives the pipeline above 1010 rows and a split gives it all
    /// 5445, and those rows carry two wide URLs apiece through a project and a top n that run on
    /// one thread. Partitioning made the aggregate itself scale and handed the difference straight
    /// back.
    ///
    /// The multiplier is [`RADIX_PARTITIONS`] because that is how many tables a split makes, and it
    /// is worth knowing that the two have moved together once already. #1000 raised the partition
    /// count from sixteen to sixty four so that a merge could run on more threads, which quadrupled
    /// this threshold as a side effect and took every aggregate under a bound of a thousand from
    /// splitting at sixteen thousand groups to splitting at sixty four thousand. #486 has the
    /// measurement that found it.
    fn partition_from(&self) -> usize {
        match self.top_counts {
            Some((bound, _)) => PARTITION_FROM.max(bound.saturating_mul(self.merged.len())),
            None => PARTITION_FROM,
        }
    }

    /// Turns partitioning on for every instance, and puts whatever was already combined where it
    /// now belongs.
    ///
    /// Called by the first instance to outgrow [`PARTITION_FROM`]. By then another instance may
    /// already have finished and left its whole table in partition zero, and that table covers every
    /// partition, so it has to be handed over before anything else lands beside it.
    ///
    /// Doing nothing when the flag is already set is not just an optimisation. Two instances can
    /// cross the threshold at the same moment, and the second one must not scatter what the first
    /// one has started folding into.
    ///
    /// This is also where the aggregate decides, once, whether to keep tables locally at all. The
    /// decision belongs here because it is the last moment at which nothing has been kept locally
    /// yet, and an aggregate that says no here never has to unpick anything later.
    fn begin_partitioning(
        &self,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        let mut built = self.built.lock().map_err(poisoned)?;
        if built.partitioning {
            return Ok(());
        }
        built.partitioning = true;
        let seeded = self.merged[0].lock().map_err(poisoned)?.table.take();
        drop(built);
        if !self.worth_local() {
            self.give_up_local(spreading)?;
        }
        match seeded {
            Some(seeded) => self.hand(seeded, spreading, own),
            None => Ok(()),
        }
    }

    /// One whole table given up, into this instance's own partitions or into the shared ones.
    ///
    /// Which of the two is the question [`Built::local`] answers, and two things answer it by
    /// themselves. A table that has spilled cannot be merged with another table at the end. And an
    /// instance that reaches here because the budget is already crowded is an instance that has no
    /// room for one set of tables per thread, which is what keeping them locally costs. Either way
    /// the aggregate stops being local here and everything already kept locally goes with it.
    fn hand(
        &self,
        from: Building,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        if from.over.is_some() || crowded(&self.memory) {
            self.give_up_local(spreading)?;
            self.hand_all(spreading, own)?;
        }
        if self.locally.load(Ordering::Relaxed) {
            return self.scatter_own(from, own);
        }
        self.hand_over(from, spreading)
    }

    /// Everything this instance was keeping to itself, folded into the shared partitions.
    fn hand_all(&self, spreading: &mut Spreading, own: &mut [Option<Building>]) -> Result<()> {
        for held in own.iter_mut() {
            let Some(table) = held.take() else { continue };
            self.hand_over(table, spreading)?;
        }
        Ok(())
    }

    /// Whether this instance can go on keeping a table of its own per partition.
    ///
    /// Asked before the chunk is folded and not after, which matters: a table that runs out of room
    /// during a fold opens a spill file, and a file opened by a table holding a hundred groups is a
    /// file that no number of later passes will get through. The check has to happen while there is
    /// still room to be wrong about.
    ///
    /// Four things end it. Three of them are about room. A table that has opened a spill file can
    /// never be merged with another table, because a key can be in one table and in the other's
    /// file at once and the merge would finish a group the file is still holding rows for. A budget
    /// already half spent is the same crowding the single table path watches for. The aggregate
    /// itself has to be small enough that one set of tables per instance still fits, which is what
    /// [`Aggregate::room_for_local`] asks.
    ///
    /// The fourth is about the cache, and it is three questions rather than one, because a set of
    /// copies is only worth giving up when it is too large to sit in the cache and is actually a
    /// set of copies. [`cache_holds_local`](Aggregate::cache_holds_local) asks the first, and
    /// [`copies_overlap`] and [`keys_arrive_together`] between them ask the second, one from the
    /// number of rows a group has and one from where in the input those rows are. All three have to
    /// say so before the tables go.
    ///
    /// Once per chunk rather than once per row, and the answer is almost always yes.
    fn still_local(
        &self,
        folded: u64,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<bool> {
        // flatten: a partition this instance has not folded into has no table and nothing to say.
        let spilled = own.iter().flatten().any(|table| table.over.is_some());
        let mine: u64 = own
            .iter()
            .flatten()
            .map(|table| table.scratch.bytes() + table.containers.bytes())
            .sum();
        let groups: u64 = own.iter().flatten().map(|table| table.groups as u64).sum();
        if !spilled
            && !crowded(&self.memory)
            && self.room_for_local(mine)
            && (self.cache_holds_local(mine)
                || !copies_overlap(folded, groups)
                || keys_arrive_together(spreading))
        {
            return Ok(true);
        }
        self.give_up_local(spreading)?;
        self.hand_all(spreading, own)?;
        Ok(false)
    }

    /// Whether this aggregate should keep tables locally at all, asked once and never again.
    ///
    /// Keeping a table per instance per partition costs the number of instances times what one set
    /// of tables holds, and that is a bet made at the moment the first table is split, before
    /// anything is known about how many groups are coming. The bet is only worth making when losing
    /// it is cheap, and losing it is cheap only when the budget is nowhere near spent.
    ///
    /// So the question asked here is whether what the query is holding right now, multiplied by the
    /// instances that would each hold their own copy of it, still fits in half the budget. That is a
    /// deliberately pessimistic reading: most of what the query holds at this point belongs to the
    /// scan and not to the aggregate, and no instance is going to duplicate the scan. The pessimism
    /// is the point. An aggregate that backs out of local mode later has to fold every local table
    /// into the shared ones while both are alive, which is the most memory the query will ever want,
    /// and it wants it at exactly the moment it is already short. Better to never start.
    ///
    /// The instance count is the one thing here that is not known yet. The first table splits long
    /// before the last instance has started, so a count read now would say one when the answer turns
    /// out to be thirty two, and the bet would be sized against a thread count that never existed.
    /// The number of partitions stands in as a floor instead, because an aggregate worth running on
    /// several threads is one the pipeline gives at least that many.
    ///
    fn worth_local(&self) -> bool {
        // String groups retain their payload in every worker's table until the merge.
        // Sharing radix partitions keeps one copy of each URL key instead.
        if self.keys.iter().any(|&key| self.plan.expr_type(key) == &LogicalType::Varchar) {
            return false;
        }
        let Some(limit) = self.memory.limit() else { return true };
        let instances = self.started.load(Ordering::Relaxed).max(self.merged.len()) as u64;
        self.memory.used().saturating_mul(instances) < limit / 2
    }

    /// Whether the budget still has room for one set of tables per instance.
    ///
    /// What this instance is holding locally is what every other instance is holding too, near
    /// enough, because the keys are divided by a hash and a hash spreads them. So the room the
    /// aggregate needs is what this one is using times the number of instances, and the question is
    /// whether that much still fits inside half the budget. Half and not all of it, because the
    /// tables have to be folded into the shared ones while both sets are alive.
    ///
    /// This asks about the tables rather than about the process, which is the difference that makes
    /// it usable. `crowded` reads what the whole query is holding, and on a scan of a large table
    /// most of that is the table and none of it is the aggregate's to give back.
    fn room_for_local(&self, mine: u64) -> bool {
        let Some(limit) = self.memory.limit() else { return true };
        let instances = self.started.load(Ordering::Relaxed) as u64;
        mine.saturating_mul(instances) < limit / 4
    }

    /// Whether one set of tables per instance still fits in the cache.
    ///
    /// The budget question above and this one look alike and are not the same question. A set of
    /// tables per instance can sit inside a twenty gigabyte budget with room to spare and still be
    /// far too large to be touched at random, and a hash aggregate is nothing but random touches: a
    /// bucket, then the key the bucket points at, then the accumulator beside it, three addresses
    /// nothing knew until the one before it landed. Once those addresses stop coming out of the
    /// cache each of them is a trip to memory, and the instances are not sharing the trips, they
    /// are competing for the same cache with a private copy each.
    ///
    /// The shared tables hold what the private ones hold between them, so what this decides is
    /// whether the aggregate's working set is one copy or as many copies as there are threads.
    /// Below the line the copies are free and the locks are not worth taking. Above it the copies
    /// are the whole cost and the locks are cheap beside them.
    ///
    /// `GROUP BY l_partkey` over TPC-H SF1 lineitem is what put the number here. It is six million
    /// rows into two hundred thousand groups, which is about sixteen megabytes of tables, and the
    /// fold costs 23 nanoseconds a row on one thread and 182 on ten, so the tenth thread was making
    /// the work slower rather than faster and the aggregate scaled 1.3 times over ten threads. The
    /// cache on the machine that was measured on holds about sixteen megabytes, and sixteen
    /// megabytes across every instance is where the sweep put the line: below it the private tables
    /// are as fast as they always were, and above it they are not tables any more, they are ten
    /// copies of one table taking turns being evicted.
    fn cache_holds_local(&self, mine: u64) -> bool {
        let instances = self.started.load(Ordering::Relaxed) as u64;
        mine.saturating_mul(instances) <= LOCAL_CACHE
    }

    /// The aggregate stops keeping a table per instance per partition, for good.
    ///
    /// The `built` lock is held across the drain, and an instance takes that same lock before it
    /// hands its tables in. That ordering is the whole of the safety here: a table cannot be
    /// deposited into a partition after this has finished looking at it, so no table is left
    /// waiting to be merged into a partition that has since started spilling.
    fn give_up_local(&self, spreading: &mut Spreading) -> Result<()> {
        let mut built = self.built.lock().map_err(poisoned)?;
        if !built.local {
            return Ok(());
        }
        built.local = false;
        self.locally.store(false, Ordering::Relaxed);
        let mut handed = Vec::new();
        for partition in &self.merged {
            handed.append(&mut partition.lock().map_err(poisoned)?.pending);
        }
        drop(built);
        for table in handed {
            self.hand_over(table, spreading)?;
        }
        Ok(())
    }

    /// This instance's own tables handed in at the end of it.
    ///
    /// They are left to be merged when the aggregate is still local and folded in now when it is
    /// not, and the `built` lock is what tells the two apart. See [`Aggregate::give_up_local`] for
    /// why that lock and not this instance's own view of the flag.
    fn deposit(&self, own: &mut [Option<Building>], spreading: &mut Spreading) -> Result<()> {
        // flatten: an instance that never partitioned has no tables here and nothing to hand in.
        if own.iter().flatten().next().is_none() {
            return Ok(());
        }
        let built = self.built.lock().map_err(poisoned)?;
        if built.local {
            for (at, held) in own.iter_mut().enumerate() {
                let Some(table) = held.take() else { continue };
                self.merged[at].lock().map_err(poisoned)?.pending.push(table);
            }
            return Ok(());
        }
        drop(built);
        self.hand_all(spreading, own)
    }

    /// One instance's own table given up to the partitions, spill file and all.
    ///
    /// The groups still in the table are scattered, which puts each of them in the one partition its
    /// hash picks. What was written out because the table could not hold it is read back and spread
    /// the same way the chunks are, because a spill file holds rows rather than aggregate states and
    /// a row is a row wherever it came from.
    ///
    /// That is the whole of why the file can be drained here rather than being a refusal. The rows
    /// in it are exactly the rows whose key the table had no room for, so folding them into the
    /// partitions adds each of them once, to the same group they would have joined had there been
    /// room. A partition may well spill again afterwards, and that is fine, because a partition's
    /// file only ever holds keys of that partition and `finalize` knows how to finish it.
    fn hand_over(&self, mut from: Building, spreading: &mut Spreading) -> Result<()> {
        let leftover = from.over.take();
        self.scatter(from, spreading.spin)?;
        let Some(mut file) = leftover else { return Ok(()) };
        let mut spilled = Spilled::new(file.read()?, self.spilled_types());
        while let Some(rows) = spilled.next(self)? {
            self.spread(&rows, spreading)?;
        }
        Ok(())
    }

    /// One batch of rows divided by the high bits of its group hash, ready to be folded.
    ///
    /// Both halves of the fold want the same three things and neither wants to hash twice, so the
    /// division is here and what is done with the pieces is not. The gather is a copy of every
    /// column of the batch and it happens before any lock is taken by either caller.
    fn split(&self, rows: &Rows, spreading: &mut Spreading) -> Result<Vec<Option<Rows>>> {
        let Spreading { hashes, picks, keyed, spin, split_rows, runs, .. } = spreading;
        crate::table::hash(&rows.keys, rows.rows, hashes, crate::table::Across::OneInput);
        for pick in picks.iter_mut() {
            pick.clear();
        }
        for hashed in keyed.iter_mut() {
            hashed.clear();
        }
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        let mut before: Option<u64> = None;
        for (row, &hash) in hashes.iter().enumerate() {
            if before != Some(hash) {
                *runs += 1;
                before = Some(hash);
            }
            let partition = (hash >> shift) as usize;
            picks[partition].push(row as u32);
            keyed[partition].push(hash);
        }
        *split_rows += rows.rows as u64;
        let mut ready: Vec<Option<Rows>> = Vec::with_capacity(RADIX_PARTITIONS);
        for pick in picks.iter() {
            ready.push(if pick.is_empty() { None } else { Some(rows.gather(pick)?) });
        }
        *spin = (*spin + 1) % RADIX_PARTITIONS;
        Ok(ready)
    }

    /// One batch of rows folded into the tables this instance keeps to itself.
    ///
    /// No lock and no sweep, because no other thread can reach any of these tables. That is the
    /// whole of the difference between this and [`Aggregate::spread`], and on ClickBench it is
    /// about half of what a grouped aggregate over a high cardinality key used to spend: the
    /// operator's wall time was twice its CPU time, and the difference was threads waiting for a
    /// partition somebody else was folding into.
    ///
    /// What it costs is a group seen by four instances held in four tables until
    /// [`Aggregate::close`] merges them. That merge is one probe per group rather than per row, it
    /// runs on as many threads at once as there are partitions, and the tables it merges are only
    /// ever the ones belonging to a single partition.
    fn spread_own(
        &self,
        rows: &Rows,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        // Timed here rather than around each `fold` below, because there is one of those per
        // partition to a chunk and a pair of clock readings on each would be a measurable share of
        // what they measure. One reading a chunk is the granularity rule the stage clock is written
        // to.
        let timing = stage::Timing::start(Stage::Fold);
        let spread = self.spreading_own(rows, spreading, own);
        timing.stop(0);
        spread
    }

    /// [`Aggregate::spread_own`] with the clock taken off it, so that the clock wraps all of it.
    fn spreading_own(
        &self,
        rows: &Rows,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        let ready = self.split(rows, spreading)?;
        for (partition, selected) in ready.iter().enumerate() {
            let Some(selected) = selected else { continue };
            let table = own[partition].get_or_insert_with(|| self.start());
            if let Some(error) = table.failure.take() {
                return Err(error);
            }
            self.fold(selected, table, Some(&spreading.keyed[partition]))?;
        }
        Ok(())
    }

    /// One whole table's groups divided among the tables this instance keeps to itself.
    ///
    /// The unlocked twin of [`Aggregate::scatter`], and simpler for two reasons. Nothing here has
    /// spilled, because a table that spills is what ends local mode, so there is no partition to
    /// set groups aside from. And nobody else can be folding into the destination, so there is no
    /// rotating start and no second sweep.
    fn scatter_own(&self, from: Building, own: &mut [Option<Building>]) -> Result<()> {
        let timing = stage::Timing::start(Stage::Scatter);
        let scattered = self.scattering_own(from, own);
        timing.stop(0);
        scattered
    }

    /// [`Aggregate::scatter_own`] with the clock taken off it, so that the clock wraps all of it.
    fn scattering_own(&self, from: Building, own: &mut [Option<Building>]) -> Result<()> {
        debug_assert!(from.over.is_none(), "a spilled table is never scattered locally");
        let Building {
            scratch,
            containers,
            table: source,
            states: taken,
            counts: tallies,
            compact: packed,
            overflow: wide,
            seen: mut watched,
            groups: found,
            ..
        } = from;
        let calls = self.calls.len();
        let distinct: Vec<bool> = self.calls.iter().map(|call| call.distinct).collect();
        let mut coming = Folding {
            count_only: self.count_only,
            calls,
            distinct: &distinct,
            taken: &taken,
            tallies: &tallies,
            compact: &packed,
            overflow: &wide,
            watched: &mut watched,
        };
        let types: Vec<LogicalType> =
            self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); RADIX_PARTITIONS];
        for start in (0..found).step_by(VECTOR_SIZE) {
            let end = (start + VECTOR_SIZE).min(found);
            let mut keys = Vec::with_capacity(types.len());
            for (at, ty) in types.iter().enumerate() {
                keys.push(source.column(at, ty, start..end)?);
            }
            for bucket in &mut buckets {
                bucket.clear();
            }
            for slot in start..end {
                buckets[(source.hash_of(slot) >> shift) as usize].push(slot);
            }
            for (at, bucket) in buckets.iter().enumerate() {
                if bucket.is_empty() {
                    continue;
                }
                let into = own[at].get_or_insert_with(|| self.start());
                let grown = self.fold_slots(&mut coming, &source, &keys, start, bucket, into)?;
                charge(into, grown)?;
            }
        }
        drop(watched);
        drop(taken);
        drop(tallies);
        drop(source);
        drop(scratch);
        drop(containers);
        Ok(())
    }

    /// One batch of rows split by the high bits of its group hash and folded into the partitions.
    ///
    /// The two sweeps are what makes this scale. Gathering a partition's rows out of the batch is a
    /// copy of every column and it happens before any lock is taken, so an instance never holds a
    /// partition while it copies. Then the partitions are tried in turn from a rotating start, and
    /// one that is already being folded into is put aside rather than waited for. The second sweep
    /// waits for what is left, by which time whoever held it has usually moved on. Without this,
    /// every instance asked for partition zero first and thirty two threads queued behind one lock
    /// before doing any work at all.
    fn spread(&self, rows: &Rows, spreading: &mut Spreading) -> Result<()> {
        // Timed as a whole for the reason [`Aggregate::spread_own`] gives, and the lock waits in
        // here are part of what it is worth timing: this is the shared path, so a chunk that spent
        // its time waiting for a partition spent it inside this clock.
        let timing = stage::Timing::start(Stage::Fold);
        let spread = self.spreading(rows, spreading);
        timing.stop(0);
        spread
    }

    /// [`Aggregate::spread`] with the clock taken off it, so that the clock wraps all of it.
    fn spreading(&self, rows: &Rows, spreading: &mut Spreading) -> Result<()> {
        let ready = self.split(rows, spreading)?;
        let Spreading { keyed, spin, waiting, .. } = spreading;
        let spin = &*spin;
        waiting.clear();
        for step in 0..RADIX_PARTITIONS {
            let partition = (step + *spin) % RADIX_PARTITIONS;
            let Some(selected) = &ready[partition] else { continue };
            match self.merged[partition].try_lock() {
                Ok(mut held) => {
                    let table = held.table.get_or_insert_with(|| self.start());
                    self.fold(selected, table, Some(&keyed[partition]))?;
                }
                Err(TryLockError::WouldBlock) => waiting.push(partition),
                Err(TryLockError::Poisoned(error)) => return Err(poisoned(error)),
            }
        }
        for &partition in waiting.iter() {
            let selected =
                ready[partition].as_ref().expect("only a filled partition was put aside");
            let mut held = self.merged[partition].lock().map_err(poisoned)?;
            let table = held.table.get_or_insert_with(|| self.start());
            self.fold(selected, table, Some(&keyed[partition]))?;
        }
        Ok(())
    }

    /// One `DISTINCT` call over a chunk, which is the one shape that still needs a value per row.
    ///
    /// `DISTINCT` inside an aggregate is a grouping of its own, one set per group per call, and the
    /// set is keyed on the same `Key` grouping was keyed on before #237. Giving it the table in
    /// `table.rs` is a change of its own and is deliberately not this one, so this is the row loop
    /// that used to be the whole of the aggregate, kept for the calls that need it.
    ///
    /// What comes back is what the values copied into the sets own away from themselves, which the
    /// caller charges once for the chunk.
    fn distinct(
        &self,
        states: &mut [Accumulator],
        seen: &mut [DistinctSet],
        rows: &Rows,
        slots: &[usize],
        at: usize,
        given: &mut [Key],
    ) -> Result<u64> {
        let calls = self.calls.len();
        let mut aside = 0;
        // row at a time: a set of rows is what `DISTINCT` is, and the table that would replace this
        // one is the one #237 built for grouping. Until that is shared, this is the honest loop.
        for (row, &slot) in slots.iter().enumerate() {
            if slot == NOWHERE {
                continue;
            }
            if let Some(flags) = &rows.filters[at] {
                if !is_true(&flags.value_at(row)) {
                    continue;
                }
            }
            if let (DistinctSet::BigInt(set), [column]) =
                (&mut seen[slot * calls + at], rows.arguments[at].as_slice())
            {
                if column.is_null_at(row) {
                    continue;
                }
                let value = match column.signed_at(row) {
                    Some(value) => i64::try_from(value)
                        .map_err(|_| Error::internal("a BIGINT distinct value is out of range"))?,
                    None => match column.try_value_at(row)? {
                        Value::BigInt(value) => value,
                        value => {
                            return Err(Error::internal(format!(
                                "a BIGINT distinct set was given {value:?}"
                            )));
                        }
                    },
                };
                if set.insert(value) {
                    aside += width_of(size_of::<i64>() * 2);
                    states[slot * calls + at].update(&[Value::BigInt(value)])?;
                }
                continue;
            }
            let args = &mut given[at];
            fill(args, &rows.arguments[at], row)?;
            // Asked before it is added, because the answer is usually that it is there already and
            // a set that is asked never takes a copy of what it was asked about. A
            // `count(DISTINCT x)` over a million rows and a thousand values copies a thousand times
            // rather than a million.
            let DistinctSet::Row(set) = &mut seen[slot * calls + at] else {
                return Err(Error::internal("a distinct set did not match its argument"));
            };
            if set.contains(args) {
                continue;
            }
            // The copy and not the buffer, because the buffer keeps whatever the longest value it
            // has ever held needed and the charge counts capacity.
            let stored = args.clone();
            aside += rows::footprint(&stored.0);
            set.insert(stored);
            states[slot * calls + at].update(&args.0)?;
        }
        Ok(aside)
    }

    /// A fresh accumulator per call, appended for the group that has just arrived.
    ///
    /// One flat vector of accumulators rather than a vector per group, so that a new group costs a
    /// push and not a trip to the allocator. The accumulators of the group in `slot` are the run of
    /// `calls` entries starting at `slot * calls`.
    fn fresh(
        &self,
        states: &mut Vec<Accumulator>,
        counts: &mut Vec<i64>,
        compact: &mut Vec<CompactNumeric>,
    ) -> Result<()> {
        if self.count_only {
            counts.push(0);
            return Ok(());
        }
        if self.compact_numeric {
            compact.push(CompactNumeric::default());
            return Ok(());
        }
        for call in &self.calls {
            states.push(Accumulator::new(&call.name, &call.returns)?);
        }
        Ok(())
    }

    fn fresh_seen(&self, seen: &mut Vec<DistinctSet>) {
        for call in &self.calls {
            let big_int = call.distinct
                && call.args.len() == 1
                && self.plan.expr_type(call.args[0]) == &LogicalType::BigInt;
            if big_int {
                seen.push(DistinctSet::BigInt(BigIntDistinct::default()));
            } else {
                seen.push(DistinctSet::Row(RowSet::default()));
            }
        }
    }

    /// The columns a spilled row is made of, in the order [`put_away`] writes them.
    ///
    /// The group key, then every argument of every call, then one column per call that has a
    /// `FILTER`. What goes out is what the row loop reads and not the input row, because the input
    /// row is wider than this almost always and because a second pass over a spilled row would
    /// otherwise have to evaluate the group and argument expressions again against a chunk it would
    /// have to rebuild first.
    ///
    /// The types come off the plan rather than off the vectors that were evaluated, so the file is
    /// described the same way whether or not any row has been written to it yet.
    fn spilled_types(&self) -> Vec<LogicalType> {
        let mut types = Vec::new();
        for &group in &self.keys {
            types.push(self.plan.expr_type(group).clone());
        }
        for call in &self.calls {
            for &argument in &call.args {
                types.push(self.plan.expr_type(argument).clone());
            }
        }
        for call in &self.calls {
            if let Some(filter) = call.filter {
                types.push(self.plan.expr_type(filter).clone());
            }
        }
        types
    }
}

/// One chunk of rows, in the vectors the row loop reads them out of.
///
/// The same shape whether the rows came from the operator below or from a spill file, which is what
/// lets one loop serve both.
struct Rows {
    keys: Vec<Vector>,
    arguments: Vec<Vec<Vector>>,
    filters: Vec<Option<Vector>>,
    rows: usize,
}

impl Rows {
    /// How many columns one of these rows is written out as, which is
    /// [`Aggregate::spilled_types`] long.
    fn width(&self) -> usize {
        self.keys.len()
            + self.arguments.iter().map(Vec::len).sum::<usize>()
            + self.filters.iter().flatten().count()
    }

    /// Copy the selected rows into vectors one radix partition can fold independently.
    fn gather(&self, rows: &[u32]) -> Result<Self> {
        Ok(Self {
            keys: self.keys.iter().map(|column| column.gather(rows)).collect::<Result<_>>()?,
            arguments: self
                .arguments
                .iter()
                .map(|arguments| {
                    arguments.iter().map(|column| column.gather(rows)).collect::<Result<_>>()
                })
                .collect::<Result<_>>()?,
            filters: self
                .filters
                .iter()
                .map(|filter| filter.as_ref().map(|column| column.gather(rows)).transpose())
                .collect::<Result<_>>()?,
            rows: rows.len(),
        })
    }
}

/// The groups a pushed down limit keeps, shared by every instance of the aggregate.
///
/// An unordered limit over a grouping may keep any groups it likes, as long as the rows of the ones
/// it keeps are all counted. On one thread that is the first ones seen. On several it has to be the
/// same ones for everybody, or an instance drops rows of a group another instance is counting and
/// the count comes back short. This is the set they all use.
#[derive(Debug)]
struct Agreed {
    /// The keys seen so far, in the order they were first seen, while there are fewer than the limit.
    table: Table,
    /// Scratch for the probe, kept so a chunk of a thousand rows asks the allocator for nothing.
    hashes: Vec<u64>,
    /// The keys once there are as many of them as the limit, ready for an instance to install.
    keys: Option<Arc<Vec<Vector>>>,
}

/// The scratch one pipeline instance keeps between chunks, and its table when it has one of its own.
#[derive(Debug)]
pub(crate) struct Partitioned {
    mixed: group_mixed::Local,
    grouped_distinct: group_distinct::Local,
    encoded: bool,
    encoded_records: Vec<EncodedCountPartition>,
    encoded_memory: Reservation,
    radix_distinct: bool,
    radix_distinct_records: Vec<BigIntDistinctPartition>,
    radix_distinct_memory: Reservation,
    fixed: bool,
    fixed_records: Vec<FixedPartition>,
    fixed_memory: Reservation,
    fixed_blocks: FixedBlocks,
    dense: bool,
    dense_codes: Vec<Vec<u32>>,
    dense_nulls: i64,
    dense_memory: Reservation,
    /// The table this instance folds into while it still keeps its groups to itself.
    ///
    /// Every instance starts with one, because splitting a chunk sixty four ways is not free and a
    /// small aggregate never earns it back. It goes when [`Aggregate::ought_to_partition`] says the
    /// table has grown enough to be worth sharing, and from then on this is `None` and the chunks go
    /// straight into the partitions.
    single: Option<Building>,
    /// Whether the agreed keys of a pushed down limit are already in this instance's table.
    ///
    /// They go in once and they never come out, so after that the table holds as many groups as the
    /// limit and the fold opens no more. Before that it is this instance's own keys in there, every
    /// one of which is in the agreed set because it was put there on the way past.
    installed: bool,
    expressions: Scratch,
    spreading: Spreading,
    /// One table per partition, belonging to this instance and to nobody else.
    ///
    /// This is what a partitioned instance folds into while the aggregate is running locally, which
    /// is every aggregate that does not run out of room. No lock is taken to reach one, because no
    /// other thread can. What it costs is that a group seen by four threads is held four times
    /// until [`Aggregate::close`] merges the four, and what it buys is that the fold itself never
    /// waits for anybody.
    own: Vec<Option<Building>>,
    /// How many rows this instance has folded, counting the ones that went into `single`.
    ///
    /// Against the groups those rows opened it says how much a table is repeating itself, which is
    /// what [`copies_overlap`] reads it for.
    folded: u64,
}

/// The four columns a fixed width record is built from, read once per chunk rather than once per row.
///
/// `GROUP BY WatchID, ClientIP` with a `COUNT`, a `SUM` and an `AVG` over it reads four integer
/// columns and builds one record per row. That used to be four `is_null_at` calls and four
/// `signed_at` calls a row, and each of those matched on the vector's body, called into the data
/// underneath and matched again on its layout, to read a number that was already sitting in a flat
/// slice. Callgrind put the two of them together at a third of ClickBench 32.
///
/// So the columns are read as blocks. [`SignedBlock`] copies a flat run, sign extends a narrower
/// one and fills the forms the vector will not hand over a row at a time exactly as the loop used
/// to, and it answers the null question once for the whole column rather than once per row.
///
/// The buffers live for as long as the instance does, so a chunk allocates nothing for this.
#[derive(Debug, Default)]
struct FixedBlocks {
    /// The first key, the second key, the SUM argument and the AVG argument, in that order.
    held: [SignedBlock; 4],
}

impl FixedBlocks {
    /// Reads the four columns of one chunk into the buffers.
    ///
    /// # Errors
    ///
    /// A column that is not an integer in any form, which is a plan that should not have reached the
    /// fixed width exchange at all.
    fn read(&mut self, rows: usize, columns: [&Vector; 4]) -> Result<()> {
        for (held, column) in self.held.iter_mut().zip(columns) {
            held.read(rows, column)?;
        }
        Ok(())
    }

    /// Whether each of the four columns has a null anywhere in this chunk.
    fn nulled(&self) -> [bool; 4] {
        let [first, second, sum, mean] = &self.held;
        [first.nulled(), second.nulled(), sum.nulled(), mean.nulled()]
    }

    /// The four buffers cut to the length of the chunk, so the loop over them checks no bounds.
    ///
    /// # Errors
    ///
    /// A buffer shorter than the chunk, which would be a vector whose length disagreed with the
    /// chunk's.
    fn cut(&self, rows: usize) -> Result<[&[i64]; 4]> {
        let [first, second, sum, mean] = &self.held;
        Ok([first.cut(rows)?, second.cut(rows)?, sum.cut(rows)?, mean.cut(rows)?])
    }
}

/// The scratch that splitting a chunk across the partitions needs, kept between chunks.
#[derive(Debug)]
struct Spreading {
    hashes: Vec<u64>,
    /// The rows of the chunk belonging to each partition, and their hashes alongside so the fold
    /// does not hash the same keys a second time.
    picks: Vec<Vec<u32>>,
    keyed: Vec<Vec<u64>>,
    /// Where this instance begins its sweep of the partitions.
    ///
    /// Every instance used to walk them in the order zero to fifteen, so thirty two threads holding
    /// thirty two chunks all queued on partition zero, then all queued on partition one behind
    /// whoever won the first. Starting each chunk one partition further along spreads the first
    /// attempt, and [`Aggregate::spread`] then takes the ones that were busy on a second pass rather
    /// than waiting for them in place.
    spin: usize,
    /// The partitions a sweep found locked, kept here so the second pass does not allocate.
    waiting: Vec<usize>,
    /// How many rows this instance has split into partitions, and how many runs of equal keys
    /// those rows arrived in.
    ///
    /// Counted inside the loop that reads the hashes, so it is one comparison a row on a pass that
    /// was happening anyway. What it is for is in [`keys_arrive_together`].
    split_rows: u64,
    runs: u64,
}

impl Spreading {
    fn new() -> Self {
        Self {
            hashes: Vec::new(),
            picks: vec![Vec::new(); RADIX_PARTITIONS],
            keyed: vec![Vec::new(); RADIX_PARTITIONS],
            spin: 0,
            waiting: Vec::new(),
            split_rows: 0,
            runs: 0,
        }
    }
}

/// What one instance of an aggregate holds while it folds.
///
/// Everything the row loop touches is in here rather than in the operator, because every one of
/// these is written to once per row and a lock per row is not an engine. The two reservations and
/// the two counters beside them are the memory charging, which is per instance for the same reason
/// and is handed over when the instance combines.
#[derive(Debug)]
pub(crate) struct Building {
    scratch: Reservation,
    containers: Reservation,
    charged: u64,
    charged_keys: u64,
    table: Table,
    states: Vec<Accumulator>,
    counts: Vec<i64>,
    compact: Vec<CompactNumeric>,
    /// Exact totals only for groups whose SMALLINT sum does not fit in 64 bits.
    overflow: HashMap<usize, (i128, i128)>,
    seen: Vec<DistinctSet>,
    groups: usize,
    given: Vec<Key>,
    hashes: Vec<u64>,
    slots: Vec<usize>,
    /// What the batched probe walks with, kept so that a chunk allocates nothing for it.
    walk: Walk,
    /// The dictionaries `coded_map` was filled against, which is what says it still means anything.
    ///
    /// Empty when the last chunk was not one the direct map could answer, so the map is rebuilt
    /// rather than read. See [`Coded`](crate::table::Coded).
    coded_on: Vec<Arc<Vector>>,
    /// One slot per combination of codes, or [`NOWHERE`] where that combination has not been seen.
    coded_map: Vec<usize>,
    /// The rows of the last chunk the map had no slot for, in row order.
    missing: Vec<usize>,
    /// One flag per row of the last chunk, true where the row's key is the key of the row before.
    ///
    /// See [`repeats`](crate::table::repeats), which fills it, and the run path in
    /// [`Aggregate::fold`], which is the only thing that reads it.
    same: Vec<bool>,
    /// The rows of the last chunk that start a run, in row order, when the run path took the chunk.
    leaders: Vec<usize>,
    /// One slot per entry of a batch of `leaders`, which is how the batched probe answers a list.
    leader_slots: Vec<usize>,
    kept: Vec<usize>,
    affine_rows: Vec<i64>,
    over: Option<Spill>,
    away: Vec<Value>,
    /// What went wrong before any row arrived, which there is nowhere else to report from.
    failure: Option<Error>,
}

/// One group of COUNT(*), SUM(SMALLINT) and AVG(SMALLINT).
///
/// The general accumulator carries its variant and return type beside every call in every group.
/// These three calls have fixed types for the whole operator, so the group holds only their totals.
#[derive(Debug, Default, Clone)]
struct CompactNumeric {
    /// The low 63 bits are COUNT(*). The high bit records whether SUM saw a value.
    count_and_sum_seen: u64,
    sum: i64,
    mean: i64,
    mean_count: i64,
}

impl CompactNumeric {
    const SUM_SEEN: u64 = 1 << 63;
    const COUNT: u64 = Self::SUM_SEEN - 1;

    fn count(&self) -> i64 {
        (self.count_and_sum_seen & Self::COUNT) as i64
    }

    fn sum_seen(&self) -> bool {
        self.count_and_sum_seen & Self::SUM_SEEN != 0
    }

    fn totals(&self, slot: usize, overflow: &HashMap<usize, (i128, i128)>) -> (i128, i128) {
        // The length first for the reason in [`Self::add`]. This one runs once per group in the
        // merge and once more when the group is handed out.
        if overflow.is_empty() {
            return (i128::from(self.sum), i128::from(self.mean));
        }
        overflow.get(&slot).copied().unwrap_or((i128::from(self.sum), i128::from(self.mean)))
    }

    fn add(
        &mut self,
        slot: usize,
        sum: Option<i16>,
        mean: Option<i16>,
        overflow: &mut HashMap<usize, (i128, i128)>,
    ) -> Result<()> {
        let count = self
            .count()
            .checked_add(1)
            .ok_or_else(|| Error::out_of_range("a compact COUNT overflowed BIGINT"))?;
        self.count_and_sum_seen =
            count as u64 | if self.sum_seen() || sum.is_some() { Self::SUM_SEEN } else { 0 };
        self.mean_count = self
            .mean_count
            .checked_add(i64::from(mean.is_some()))
            .ok_or_else(|| Error::out_of_range("a compact AVG count overflowed BIGINT"))?;
        let added_sum = i64::from(sum.unwrap_or(0));
        let added_mean = i64::from(mean.unwrap_or(0));
        // Asked before the map is, because an empty map holds no slot and answering that from the
        // length costs a load where asking the map costs a SipHash of the slot. This runs once per
        // row and the map is empty in every query that does not overflow a SMALLINT sum past sixty
        // four bits, which needs on the order of ten to the fourteen rows in one group. It was eight
        // percent of ClickBench 32.
        if overflow.is_empty() {
            if let (Some(total_sum), Some(total_mean)) =
                (self.sum.checked_add(added_sum), self.mean.checked_add(added_mean))
            {
                self.sum = total_sum;
                self.mean = total_mean;
                return Ok(());
            }
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = overflow.entry(slot) {
            if let (Some(total_sum), Some(total_mean)) =
                (self.sum.checked_add(added_sum), self.mean.checked_add(added_mean))
            {
                self.sum = total_sum;
                self.mean = total_mean;
                return Ok(());
            }
            entry.insert((
                i128::from(self.sum) + i128::from(added_sum),
                i128::from(self.mean) + i128::from(added_mean),
            ));
            return Ok(());
        }
        let totals = overflow.get_mut(&slot).expect("a wide compact total has an overflow entry");
        totals.0 = totals
            .0
            .checked_add(i128::from(added_sum))
            .ok_or_else(|| Error::out_of_range("a compact SUM overflowed its exact total"))?;
        totals.1 = totals
            .1
            .checked_add(i128::from(added_mean))
            .ok_or_else(|| Error::out_of_range("a compact AVG overflowed its exact total"))?;
        Ok(())
    }

    fn combine(
        &mut self,
        target: usize,
        coming: &Self,
        slot: usize,
        from: &HashMap<usize, (i128, i128)>,
        into: &mut HashMap<usize, (i128, i128)>,
    ) -> Result<()> {
        let (sum, mean) = self.totals(target, into);
        let (coming_sum, coming_mean) = coming.totals(slot, from);
        let sum = sum
            .checked_add(coming_sum)
            .ok_or_else(|| Error::out_of_range("a compact SUM overflowed its exact total"))?;
        let mean = mean
            .checked_add(coming_mean)
            .ok_or_else(|| Error::out_of_range("a compact AVG overflowed its exact total"))?;
        let count = self
            .count()
            .checked_add(coming.count())
            .ok_or_else(|| Error::out_of_range("a compact COUNT overflowed BIGINT"))?;
        self.count_and_sum_seen =
            count as u64 | if self.sum_seen() || coming.sum_seen() { Self::SUM_SEEN } else { 0 };
        self.mean_count = self
            .mean_count
            .checked_add(coming.mean_count)
            .ok_or_else(|| Error::out_of_range("a compact AVG count overflowed BIGINT"))?;
        match (i64::try_from(sum), i64::try_from(mean)) {
            (Ok(sum), Ok(mean)) => {
                self.sum = sum;
                self.mean = mean;
                into.remove(&target);
            }
            _ => {
                into.insert(target, (sum, mean));
            }
        }
        Ok(())
    }
}

fn flat_smallint(column: &Vector) -> Option<&[i16]> {
    match column.data() {
        Some(Data::Int16(values)) => Some(values.as_slice()),
        _ => None,
    }
}

/// A spill file being read back, and the buffers reading it fills.
///
/// The file is rows and the row loop wants columns, so something has to turn one into the other, and
/// this is it. The buffers are kept between chunks so that a string read out of the file goes into
/// the block the string before it used rather than into a new one.
struct Spilled<'s> {
    reader: Reader<'s>,
    types: Vec<LogicalType>,
    row: Vec<Value>,
    columns: Vec<Vec<Value>>,
}

/// Values already accepted by one `DISTINCT` aggregate in one group.
#[derive(Debug)]
enum DistinctSet {
    BigInt(BigIntDistinct),
    Row(RowSet),
}

/// Signed 64-bit distinct values with the first value held inline.
///
/// High-cardinality string grouping commonly creates one group per row. A `COUNT(DISTINCT BIGINT)`
/// beside it used to allocate a hash table for every one of those singleton groups. The first value
/// needs no table, and the table is created only when a second distinct value reaches the group.
#[derive(Debug, Default)]
enum BigIntDistinct {
    #[default]
    Empty,
    One(i64),
    Many(BigIntSet),
}

impl BigIntDistinct {
    fn insert(&mut self, value: i64) -> bool {
        match self {
            Self::Empty => {
                *self = Self::One(value);
                true
            }
            Self::One(held) if *held == value => false,
            Self::One(held) => {
                let first = *held;
                let mut values = BigIntSet::default();
                values.insert(first);
                values.insert(value);
                *self = Self::Many(values);
                true
            }
            Self::Many(values) => values.insert(value),
        }
    }

    fn into_each(self, mut accept: impl FnMut(i64) -> Result<()>) -> Result<()> {
        match self {
            Self::Empty => Ok(()),
            Self::One(value) => accept(value),
            Self::Many(values) => {
                for value in values {
                    accept(value)?;
                }
                Ok(())
            }
        }
    }
}

impl<'s> Spilled<'s> {
    fn new(reader: Reader<'s>, types: Vec<LogicalType>) -> Self {
        let columns = vec![Vec::new(); types.len()];
        Self { reader, types, row: Vec::new(), columns }
    }

    /// Up to [`VECTOR_SIZE`] rows, turned back into the vectors they were written out of.
    fn next(&mut self, pass: &Aggregate<'_>) -> Result<Option<Rows>> {
        // Field by field, because the row being read and the columns it is being moved into are two
        // borrows of this and the loop below holds both.
        let Self { reader, types, row, columns } = self;
        for column in columns.iter_mut() {
            column.clear();
        }
        let mut rows = 0;
        while rows < VECTOR_SIZE && reader.next_into(row)? {
            // Moved rather than cloned. The buffer a string was read into is handed to the column
            // and the row keeps a null in its place, so the string is allocated once and copied
            // never, which is the same trade the reader itself makes.
            for (at, value) in row.iter_mut().enumerate() {
                columns[at].push(std::mem::replace(value, Value::Null));
            }
            rows += 1;
        }
        if rows == 0 {
            return Ok(None);
        }
        let mut built = Vec::with_capacity(columns.len());
        for (values, ty) in columns.iter().zip(&*types) {
            built.push(Vector::from_values(ty.clone(), values)?);
        }
        // Taken apart in the order `spilled_types` put them together in. A miscount here would hand
        // an argument to the wrong call rather than fail, so the two are written next to each other
        // on purpose.
        let mut taking = built.into_iter();
        let keys: Vec<Vector> = taking.by_ref().take(pass.keys.len()).collect();
        let mut arguments = Vec::with_capacity(pass.calls.len());
        for call in &pass.calls {
            arguments.push(taking.by_ref().take(call.args.len()).collect());
        }
        let mut filters = Vec::with_capacity(pass.calls.len());
        for call in &pass.calls {
            filters.push(if call.filter.is_some() { taking.next() } else { None });
        }
        Ok(Some(Rows { keys, arguments, filters, rows }))
    }
}

/// Writes one row of `seen` out whole.
///
/// `away` is the buffer the row goes through, kept by the caller across rows for the reason
/// [`fill`] gives: a `Value::Varchar` owns its bytes, and a spill that took a fresh buffer per
/// string would ask the allocator once per string per row.
fn put_away(file: &mut Spill, seen: &Rows, row: usize, away: &mut Vec<Value>) -> Result<()> {
    let columns = seen
        .keys
        .iter()
        .chain(seen.arguments.iter().flatten())
        .chain(seen.filters.iter().flatten());
    away.truncate(seen.width());
    for (at, column) in columns.enumerate() {
        match away.get_mut(at) {
            Some(slot) => set(slot, column, row)?,
            None => away.push(column.try_value_at(row)?),
        }
    }
    file.write(away)
}

/// How many more passes over a spill file are worth starting.
///
/// Sixty four, and the number is a bound on wasted reading rather than a guess about anything. A
/// query that fits in the budget makes one pass, a query that needs a few times the budget makes a
/// few, and a query that would read its own spill file sixty four more times is one whose answer
/// does not fit and which is going to say so eventually anyway, having read a hundred gigabytes off
/// a disk first.
const PASSES: u64 = 64;

/// Stops a pass whose spill file has grown past what the passes after it could get through.
///
/// The rows in the file are an upper bound on the keys left to finish, since a key cannot be in more
/// rows than there are, and each pass after this one finishes at most about as many keys as this one
/// did. That second half is the part that makes this a floor and not a guess: what a pass finishes
/// is held for the rest of the query, so every pass starts with less room than the one before it and
/// none of them gets faster.
///
/// # Errors
///
/// [`rudb_common::ErrorCode::OutOfMemory`], because that is what it is. The budget is too small for
/// this aggregation by a factor large enough that spilling does not close it, and saying so while
/// the file is a few megabytes is better than saying it after the file is the size of the input.
fn hopeless(file: &Spill, groups: usize) -> Result<()> {
    let left = file.rows() / width_of(groups).max(1);
    if left > PASSES {
        return Err(Error::out_of_memory(format!(
            "the memory limit leaves room for {groups} groups at a time and {} rows have already \
             gone to a spill file, which is more passes over it than this will finish in",
            file.rows()
        )));
    }
    Ok(())
}

/// Whether the table has taken enough of the budget that it should stop growing.
///
/// Half rather than all of it, and the half that is left is not slack. A pass has to turn its table
/// into rows before it can give the table back, and both are alive while it does. The rows are
/// cheaper than the table they came from, because the key is moved out of the table rather than
/// copied and what is added is a row header and the aggregate results, but cheaper is not free, and
/// a pass that grew its table until the budget was gone would fail on the conversion having already
/// done all of the work. Three quarters was tried and is where that happens.
///
/// What this does not do is bound the answer. The rows every pass finished are held until the last
/// pass ends, so a query whose output does not fit still runs out of memory, and one whose output
/// nearly fits gets fewer groups per pass and so more passes over a file it reads again each time.
/// Splitting the spill by a hash of the key, so that each part is aggregated once and independently,
/// is what makes that linear, and handing the finished rows out as they are made rather than at the
/// end is what makes the output stop counting. Both are larger than this and neither is needed to
/// stop the ten queries that fail today from failing.
///
/// A database opened without a limit never spills, which is the same answer it gives everywhere
/// else: no limit means the machine is the limit and the allocator is what says so.
fn crowded(memory: &Memory) -> bool {
    match memory.limit() {
        Some(limit) => memory.used() >= limit / 2,
        None => false,
    }
}

/// Whether the tables the instances keep to themselves are mostly copies of each other.
///
/// Two instances hold the same group only when that group has rows on both of them, and the rows
/// are dealt out a morsel at a time, so a group with one row is on one instance and a group with
/// fifty is on all of them. A hundred thousand groups of fifty rows is ten instances holding a
/// hundred thousand groups each while a shared table holds a hundred thousand between them, which
/// is ten copies of everything. A hundred thousand groups of one row is ten instances holding ten
/// thousand each and a shared table holds the same hundred thousand, so there is nothing to remove
/// and the lock would be paid for nothing.
///
/// An instance can read which of the two it is in off its own table. It folded `folded` rows and
/// they opened `groups` groups, so a group of its own has `folded / groups` rows in it. Call that
/// `m`. If the keys arrive in no particular order then a group with `k` rows in the whole input has
/// `k / t` of them here, and `m` and the share of the aggregate's groups this instance holds move
/// together: `m` of one is a table holding almost nothing twice, `m` of one point four is a table
/// holding half of every group there is, and `m` of three is a table holding all of them.
///
/// One point four is the line, which is the point where the shared table would be half the size of
/// the copies put together. TPC-H q17 groups six million lineitem rows by part key into two hundred
/// thousand and ends at three, which is the query this helps most. q20 groups a seventh of them by
/// part key and supplier key into about as many groups as it has rows and stays at one, and sharing
/// those tables costs the locks and saves nothing, which is what this is here to notice.
fn copies_overlap(folded: u64, groups: u64) -> bool {
    groups > 0 && folded.saturating_mul(5) >= groups.saturating_mul(7)
}

/// Whether the rows of a group arrive together, in which case no two instances hold the same group.
///
/// [`copies_overlap`] reads the number of rows a group has and assumes the input says nothing about
/// where they are, which is true of a key the file is not sorted on and false of one it is. TPC-H
/// lineitem is written in order key order, so the four rows of an order are next to each other, one
/// morsel gets all four and no other instance ever sees that order at all. The tables are already
/// divided between the instances by the input itself, there is nothing for a shared table to
/// remove, and `GROUP BY l_orderkey` has exactly the rows a group that [`copies_overlap`] would
/// call worth sharing.
///
/// So the aggregate counts, as it hashes each chunk, how many runs of equal keys the rows arrived
/// in. Two rows a run is enough to say the input is ordered on the key: a key the file is not sorted
/// on gives one run a row until the keys run out, and six million lineitem rows over two hundred
/// thousand part keys give one run a row, while the same rows over a million and a half order keys
/// give one run every four.
fn keys_arrive_together(spreading: &Spreading) -> bool {
    spreading.runs > 0 && spreading.split_rows >= spreading.runs.saturating_mul(2)
}

/// Fills `key` with one row of `columns`, reusing what the row before it left behind.
///
/// The point of filling rather than collecting is the strings. A `Value::Varchar` owns its bytes, so
/// reading a string column a row at a time takes a buffer from the allocator on every row and gives
/// it back on the next one, and a group by over `URL` does that a hundred million times to look at
/// each buffer once. Writing into the buffer that is already there asks for nothing. Every other
/// value owns nothing, so overwriting one is a move of a few bytes.
fn fill(key: &mut Key, columns: &[Vector], row: usize) -> Result<()> {
    key.0.truncate(columns.len());
    for (at, column) in columns.iter().enumerate() {
        match key.0.get_mut(at) {
            Some(slot) => set(slot, column, row)?,
            None => key.0.push(column.try_value_at(row)?),
        }
    }
    Ok(())
}

/// Puts one column's value at `row` into `slot`, keeping the buffer that is already there if it can.
fn set(slot: &mut Value, column: &Vector, row: usize) -> Result<()> {
    if let (Value::Varchar(buffer), Some(text)) = (&mut *slot, column.try_text_at(row)?) {
        buffer.clear();
        buffer.push_str(text);
        return Ok(());
    }
    *slot = column.try_value_at(row)?;
    Ok(())
}

impl Sink for Aggregate<'_> {
    type Local = Partitioned;

    /// A grouped aggregate finishes on every thread the query was given.
    ///
    /// Its finish is a merge of radix partitions and nothing a partition holds depends on any other
    /// partition, so it is as wide as there are partitions to take. That is not the same width as
    /// the scan underneath it, which is cut by how many rows the file has, and the two used to be
    /// the same number because a pipeline borrowed threads for its source and then finished on
    /// those. On a thirty two thread machine `GROUP BY WatchID, ClientIP` over a million rows was
    /// merging a million groups on the sixteen threads the scan asked for.
    ///
    /// Asked for everything rather than for a guess, because this is asked before a row has been
    /// read and there is nothing here yet to guess from. An ungrouped aggregate is one group made
    /// when the instance is and has no merge to spread, so it says one and the pipeline borrows
    /// whatever its source wanted.
    fn finalize_degree(&self, ceiling: usize) -> usize {
        if self.groups.is_empty() { 1 } else { ceiling }
    }

    fn local(&self) -> Partitioned {
        self.started.fetch_add(1, Ordering::Relaxed);
        Partitioned {
            mixed: group_mixed::Local::new(&self.memory),
            grouped_distinct: group_distinct::Local::new(&self.memory),
            encoded: false,
            encoded_records: (0..RADIX_PARTITIONS)
                .map(|_| EncodedCountPartition::default())
                .collect(),
            encoded_memory: self.memory.reservation(),
            radix_distinct: false,
            radix_distinct_records: (0..RADIX_PARTITIONS)
                .map(|_| BigIntDistinctPartition::default())
                .collect(),
            radix_distinct_memory: self.memory.reservation(),
            fixed: false,
            fixed_records: (0..RADIX_PARTITIONS).map(|_| FixedPartition::default()).collect(),
            fixed_blocks: FixedBlocks::default(),
            fixed_memory: self.memory.reservation(),
            dense: false,
            dense_codes: vec![Vec::new(); DENSE_PARTITIONS],
            dense_nulls: 0,
            dense_memory: self.memory.reservation(),
            single: Some(self.start()),
            installed: false,
            expressions: self.inputs.scratch(),
            spreading: Spreading::new(),
            own: (0..RADIX_PARTITIONS).map(|_| None).collect(),
            folded: 0,
        }
    }

    /// Refused for a limit pushed down into an aggregate with no groups, and for nothing else.
    ///
    /// A pushed down limit used to be refused outright. `max_groups` stops the table opening groups
    /// once an unordered limit above cannot observe another, and every instance would stop at its
    /// own tenth group while the rows of the groups it dropped kept arriving, so `count(*)` came back
    /// short. That is #474's trick, which is worth keeping, and the price of keeping it was that the
    /// aggregate under it ran on one thread.
    ///
    /// It no longer is. [`Aggregate::agree`] settles the groups between the instances before any of
    /// them can open one of its own, so they all stop at the same tenth group and a row that one of
    /// them drops is a row all of them drop. What is refused here is a limit over an aggregate with
    /// no group expressions, where there is one slot, no key to agree on and nothing to divide.
    ///
    /// A grouped count over one column is refused as well, because that is the shape the dense count
    /// takes when the column turns out to carry a stable dictionary, and the dense count answers a
    /// partition at a time in whatever order the partitions finish. Nothing above it usually cares,
    /// because a group by over a dictionary is on its way to a sort. Under a raw limit it would
    /// decide which ten rows come out, and that is not a thing to leave to a race.
    ///
    /// Spilling used to be refused here too, because a key could be in one instance's table and in
    /// another instance's spill file at once. Partitioning answers that: a partition's file only
    /// ever holds keys belonging to that partition, so the key is either finished in the partition
    /// or absent from it, which is the invariant spilling rested on all along.
    fn parallel(&self) -> bool {
        self.max_groups.is_none() || (!self.alone && !(self.count_only && self.keys.len() == 1))
    }

    /// One chunk, either into this instance's own table or split across the shared partitions.
    ///
    /// An instance starts with a table of its own, because splitting a chunk is not free and a small
    /// aggregate never earns it back. It keeps that table until it holds [`PARTITION_FROM`] groups
    /// and there is more than one instance to share with, and then hands what it has to the
    /// partitions and folds into them from that point on.
    ///
    /// The two sweeps are what makes the partitioned half scale. Gathering a partition's rows out of
    /// the chunk is a copy of every column and it happens before any lock is taken, so an instance
    /// never holds a partition while it copies. Then the partitions are tried in turn from a rotating
    /// start, and one that is already being folded into is put aside rather than waited for. The
    /// second sweep waits for what is left, by which time the instance that held it has usually moved
    /// on. Without this, every instance asked for partition zero first and thirty two threads queued
    /// behind one lock before doing any work at all.
    fn sink(&self, chunk: &Chunk, local: &mut Partitioned) -> Result<Progress> {
        let Partitioned {
            mixed,
            grouped_distinct,
            encoded,
            encoded_records,
            encoded_memory,
            radix_distinct,
            radix_distinct_records,
            radix_distinct_memory,
            fixed,
            fixed_records,
            fixed_memory,
            fixed_blocks,
            dense,
            dense_codes,
            dense_nulls,
            dense_memory,
            single,
            installed,
            expressions,
            spreading,
            own,
            folded,
        } = local;
        let rows = self.read(chunk, expressions)?;
        if self.mixed_top_count() {
            let [group] = rows.keys.as_slice() else {
                return Err(Error::internal("a mixed radix exchange received the wrong key width"));
            };
            let [sum] = rows.arguments[0].as_slice() else {
                return Err(Error::internal("a mixed radix exchange received no SUM argument"));
            };
            let [mean] = rows.arguments[2].as_slice() else {
                return Err(Error::internal("a mixed radix exchange received no AVG argument"));
            };
            let [user] = rows.arguments[3].as_slice() else {
                return Err(Error::internal(
                    "a mixed radix exchange received no distinct argument",
                ));
            };
            let buffered = group_mixed::Exchange::buffer(
                &self.mixed,
                &self.memory,
                [group, sum, mean, user],
                rows.rows,
                mixed,
            );
            buffered?;
            return Ok(Progress::More);
        }
        if self.grouped_distinct_top_count() {
            if rows.keys.len() != self.keys.len() {
                return Err(Error::internal(
                    "a grouped distinct exchange received the wrong key width",
                ));
            }
            let Some(user) = rows.arguments.first().and_then(|arguments| arguments.first()) else {
                return Err(Error::internal("a grouped distinct exchange received no argument"));
            };
            // What each group column is read as, which for a string key is its dictionary code when
            // the column brought one and nothing at all when it did not. A dictionary that holds a
            // null is left out: a code would then stand for a null as well as the key's own
            // validity does, and two ways of being null in one group column is a way to get the
            // count wrong.
            let keys: Vec<group_distinct::Key<'_>> = rows
                .keys
                .iter()
                .zip(&self.keys)
                .map(|(vector, &key)| {
                    let kind = self.plan.expr_type(key);
                    let codes = if kind == &LogicalType::Varchar {
                        match vector.stable_dictionary_parts() {
                            Some((codes, dictionary))
                                if !dictionary.validity().has_nulls(dictionary.len()) =>
                            {
                                group_distinct::Codes::Dictionary(codes, dictionary)
                            }
                            _ => group_distinct::Codes::Loose,
                        }
                    } else {
                        group_distinct::Codes::Signed
                    };
                    group_distinct::Key { vector, kind, codes }
                })
                .collect();
            let timing = stage::Timing::start(Stage::Scatter);
            let buffered = group_distinct::Exchange::buffer(
                &self.grouped_distinct,
                &keys,
                user,
                rows.rows,
                grouped_distinct,
            );
            timing.stop(0);
            if buffered? {
                return Ok(Progress::More);
            }
        }
        if self.encoded_top_count() {
            let timing = stage::Timing::start(Stage::Scatter);
            let buffered = self.buffer_encoded_count(&rows, encoded_records, encoded_memory);
            timing.stop(0);
            if buffered? {
                *encoded = true;
                return Ok(Progress::More);
            }
        }
        if self.radix_distinct_count {
            let timing = stage::Timing::start(Stage::Scatter);
            let buffered =
                self.buffer_bigint_distinct(&rows, radix_distinct_records, radix_distinct_memory);
            timing.stop(0);
            buffered?;
            *radix_distinct = true;
            return Ok(Progress::More);
        }
        if self.fixed_top_count() {
            let timing = stage::Timing::start(Stage::Scatter);
            let buffered = self.buffer_fixed(&rows, fixed_records, fixed_memory, fixed_blocks);
            timing.stop(0);
            buffered?;
            *fixed = true;
            return Ok(Progress::More);
        }
        if self.count_only && self.keys.len() == 1 {
            if let [key] = rows.keys.as_slice() {
                if let Some((codes, dictionary)) = key.stable_dictionary_parts() {
                    let state = self.dense.get_or_init(|| DenseCount {
                        dictionary: Arc::clone(dictionary),
                        partitions: (0..DENSE_PARTITIONS)
                            .map(|_| Mutex::new(DensePartition::default()))
                            .collect(),
                        held: Mutex::new(Vec::new()),
                    });
                    if !Arc::ptr_eq(&state.dictionary, dictionary) {
                        return Err(Error::internal(
                            "one stable dictionary aggregate received two code spaces",
                        ));
                    }
                    let validity = key.validity();
                    let before = dense_codes.iter().map(Vec::capacity).sum::<usize>();
                    if !validity.has_nulls(rows.rows)
                        && !dictionary.validity().has_nulls(dictionary.len())
                    {
                        for &code in &codes[..rows.rows] {
                            if code as usize >= dictionary.len() {
                                return Err(Error::internal(
                                    "a stable dictionary code is out of range",
                                ));
                            }
                            dense_codes[code as usize % DENSE_PARTITIONS].push(code);
                        }
                    } else {
                        for (row, &code) in codes.iter().enumerate().take(rows.rows) {
                            if key.is_null_at(row) {
                                *dense_nulls += 1;
                            } else {
                                let code = code as usize;
                                if code >= dictionary.len() {
                                    return Err(Error::internal(
                                        "a stable dictionary code is out of range",
                                    ));
                                }
                                dense_codes[code % DENSE_PARTITIONS].push(code as u32);
                            }
                        }
                    }
                    let after = dense_codes.iter().map(Vec::capacity).sum::<usize>();
                    dense_memory.grow(width_of(after.saturating_sub(before) * size_of::<u32>()))?;
                    *dense = true;
                    return Ok(Progress::More);
                }
            }
        }
        *folded += rows.rows as u64;
        if let Some(table) = single {
            if let Some(error) = table.failure.take() {
                return Err(error);
            }
            if let Some(limit) = self.max_groups {
                if !self.alone {
                    self.agree(&rows, limit, table, installed)?;
                }
            }
            let timing = stage::Timing::start(Stage::Fold);
            let folded = self.fold(&rows, table, None);
            timing.stop(0);
            folded?;
            if !self.ought_to_partition(table) {
                return Ok(Progress::More);
            }
            let handing = single.take().expect("the table was there a moment ago");
            self.begin_partitioning(spreading, own)?;
            self.hand(handing, spreading, own)?;
            return Ok(Progress::More);
        }
        if self.locally.load(Ordering::Relaxed) && self.still_local(*folded, spreading, own)? {
            self.spread_own(&rows, spreading, own)?;
            return Ok(Progress::More);
        }
        self.spread(&rows, spreading)?;
        Ok(Progress::More)
    }

    /// The end of one instance, which is nothing at all if it was already partitioning.
    ///
    /// An instance that still holds a table has to give it up here, and where it goes depends on
    /// whether anybody has started partitioning. If nobody has, it goes into partition zero whole or
    /// is merged into what is already there, which is the line merge this had before any of this and
    /// is what a query small enough never to partition still does. If somebody has, it is scattered,
    /// because a group sitting in partition zero whole while a copy of it sits in partition five as
    /// part of a split would be two rows in the answer.
    ///
    /// The flag is read under the `built` lock and the table in partition zero is taken under it too,
    /// which is the lock order [`Aggregate::begin_partitioning`] keeps as well. That is what decides
    /// the race: an instance that sets the flag and takes the table cannot interleave with one that
    /// reads the flag and takes the table, so exactly one of them ends up holding it.
    ///
    /// The merge itself runs with both locks dropped, and the flag is read again afterwards. An
    /// instance that was merging while somebody else turned partitioning on sees it on the next time
    /// round and hands the merged table over, so the table is never left whole in partition zero
    /// after the switch.
    ///
    /// Merging outside the lock is what makes the close scale. It used to happen with `built` and
    /// partition zero both held, so sixteen instances merged one at a time however many threads the
    /// query had, and the last one to arrive waited for the fifteen before it. Now an instance that
    /// finds partition zero empty leaves its table there and goes, and an instance that finds one
    /// takes it and merges the pair on its own thread, so the tables come together in a tree. On
    /// ClickBench 11, which groups two columns down to 143 and so never partitions, the pipeline
    /// spent 0.645 of its slowest instance's 2.329 milliseconds off CPU waiting for that queue.
    ///
    /// A tree copies more than a queue does. A queue folds each instance's rows into the accumulator
    /// once, and a tree folds the result of one merge into the next, so a row is carried up as many
    /// levels as the tree is deep. What pays for that is the depth: a queue is as many merges long as
    /// there are instances and a tree is the logarithm of that, and the cost of one merge here is
    /// bounded by the group count rather than by the level, because instances that between them hold
    /// fewer groups than [`PARTITION_FROM`] hold mostly the same ones. The bound is also why the
    /// extra copying is small in absolute terms: an aggregate that reaches this line has fewer than
    /// [`PARTITION_FROM`] groups in total, so there is not much in any of the tables to carry.
    fn combine(&self, local: Partitioned) -> Result<()> {
        let Partitioned {
            mixed,
            grouped_distinct,
            encoded,
            mut encoded_records,
            encoded_memory,
            radix_distinct,
            mut radix_distinct_records,
            radix_distinct_memory,
            fixed,
            mut fixed_records,
            fixed_memory,
            dense,
            mut dense_codes,
            dense_nulls,
            dense_memory,
            single,
            mut spreading,
            mut own,
            ..
        } = local;
        if mixed.used() {
            let state = self.mixed.get().expect("a mixed exchange exists after its sink");
            state.combine(mixed)?;
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if grouped_distinct.used() {
            let state = self
                .grouped_distinct
                .get()
                .and_then(Option::as_ref)
                .expect("a grouped distinct exchange exists after its sink buffered a chunk");
            state.combine(grouped_distinct)?;
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if encoded {
            let state = self
                .encoded_count
                .get()
                .and_then(Option::as_ref)
                .expect("an encoded exchange exists after an encoded sink");
            for (partition, rows) in encoded_records.iter_mut().enumerate() {
                if rows.rows.is_empty() {
                    continue;
                }
                let run = std::mem::take(rows);
                state.partitions[partition].lock().map_err(poisoned)?.runs.push(run);
            }
            state.held.lock().map_err(poisoned)?.push(encoded_memory);
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if radix_distinct {
            let state = self
                .bigint_distinct
                .get()
                .expect("a distinct exchange exists after a distinct sink");
            for (partition, rows) in radix_distinct_records.iter_mut().enumerate() {
                if rows.rows.is_empty() {
                    continue;
                }
                let run = std::mem::take(&mut rows.rows);
                state.partitions[partition].lock().map_err(poisoned)?.runs.push(run);
            }
            state.held.lock().map_err(poisoned)?.push(radix_distinct_memory);
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if fixed {
            let state = self.fixed.get().expect("fixed exchange exists after a fixed sink");
            for (partition, rows) in fixed_records.iter_mut().enumerate() {
                if rows.rows.is_empty() {
                    continue;
                }
                let run = std::mem::take(rows);
                state.partitions[partition].lock().map_err(poisoned)?.runs.push(run);
            }
            state.held.lock().map_err(poisoned)?.push(fixed_memory);
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if dense {
            let state = self.dense.get().expect("dense state exists after a dense sink");
            for (partition, run) in dense_codes.iter_mut().enumerate() {
                if run.is_empty() && (partition != 0 || dense_nulls == 0) {
                    continue;
                }
                let mut shared = state.partitions[partition].lock().map_err(poisoned)?;
                if partition == 0 {
                    shared.nulls += dense_nulls;
                }
                if !run.is_empty() {
                    shared.runs.push(std::mem::take(run));
                }
            }
            state.held.lock().map_err(poisoned)?.push(dense_memory);
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        self.deposit(&mut own, &mut spreading)?;
        let mut built = self.built.lock().map_err(poisoned)?;
        built.instances += 1;
        let Some(arriving) = single else { return Ok(()) };
        if let Some(error) = arriving.failure {
            return Err(error);
        }
        if built.partitioning {
            drop(built);
            self.hand(arriving, &mut spreading, &mut own)?;
            // A second deposit, because `hand` can put the arriving table into this instance's own
            // tables rather than into the shared partitions, and the deposit above ran before it
            // and so saw nothing. Without this, an instance that never partitioned on its own but
            // finished while the aggregate was partitioning locally had its whole table scattered
            // into tables that were then dropped on the floor, and every row it had folded went
            // with them. That is the lost count #614 turned worker-local tables off for.
            //
            // Cheap when `hand` took the shared path, because a deposit of nothing is a check that
            // this instance holds no table and a return.
            return self.deposit(&mut own, &mut spreading);
        }
        drop(built);
        let mut arriving = arriving;
        loop {
            let mut built = self.built.lock().map_err(poisoned)?;
            if built.partitioning {
                // Somebody turned it on while this instance was merging, so what it is holding is
                // the wrong shape for partition zero and has to be scattered like any other table.
                drop(built);
                self.hand(arriving, &mut spreading, &mut own)?;
                return self.deposit(&mut own, &mut spreading);
            }
            let mut kept = self.merged[0].lock().map_err(poisoned)?;
            let Some(waiting) = kept.table.take() else {
                kept.table = Some(arriving);
                return Ok(());
            };
            // Two tables cannot be merged when either of them has spilled, because a key can be in
            // one table and in the other's file at once, and the merge would finish a group the file
            // is still holding rows for. Partitioning is the answer to that, so the pair turns it on
            // here rather than the merge refusing. It takes an instance that filled its budget
            // without ever reaching a chunk that would have made it partition on its own, which is
            // rare and used to be a not implemented error.
            if arriving.over.is_some() || waiting.over.is_some() {
                built.partitioning = true;
                drop(kept);
                drop(built);
                self.hand_over(waiting, &mut spreading)?;
                return self.hand_over(arriving, &mut spreading);
            }
            drop(kept);
            drop(built);
            self.merge(waiting, &mut arriving)?;
        }
    }

    /// Every instance has combined, so the partitions become the answer.
    ///
    /// On as many threads as the pipeline ran instances on, up to one per partition, because each
    /// partition holds every row of every group that hashes to it and nothing it produces depends on
    /// what any other partition holds. This used to be one thread walking sixteen partitions and
    /// building the whole answer, and on ClickBench at ten million rows that one thread was half the
    /// query: q34 spent 0.47 seconds of a 0.93 second run in here and it did not get any shorter
    /// when the threads went from eight to thirty two.
    ///
    /// The later passes of a spilled aggregate happen here. They used to happen in `combine`, which
    /// was the same moment when one instance was all there could be, and they cannot stay there now
    /// that an instance may be one of several: a pass over a spill file is a pass over the whole
    /// aggregate's leftovers and not over one thread's.
    ///
    /// An aggregate whose pipeline took no morsel at all has nothing kept and nothing to finish,
    /// which is an empty answer and not an error. An ungrouped aggregate never gets there, because
    /// its one group is made when the instance is, and an instance is made whether or not a row
    /// arrives.
    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        if let Some(mixed) = self.mixed.get() {
            let chunks = mixed.finish(
                threads,
                self.top_counts.expect("a mixed exchange has a TopN bound").0,
                &self.memory,
            )?;
            return self.out.fill(chunks);
        }
        if let Some(Some(distinct)) = self.grouped_distinct.get() {
            let chunks = distinct.finish(
                threads,
                self.top_counts.expect("a grouped distinct exchange has a TopN bound").0,
                &self.memory,
            )?;
            return self.out.fill(chunks);
        }
        if let Some(Some(encoded)) = self.encoded_count.get() {
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<Part>>>> =
                (0..RADIX_PARTITIONS).map(|_| Mutex::new(None)).collect();
            let input = encoded
                .partitions
                .iter()
                .map(|partition| {
                    partition
                        .lock()
                        .map(|runs| runs.runs.iter().map(|run| run.rows.len()).sum::<usize>())
                        .map_err(poisoned)
                })
                .sum::<Result<usize>>()?;
            let degree = degree_for(input, threads);
            let bound = self.top_counts.expect("an encoded exchange has a TopN bound").0;
            together(threads, degree, &|| {
                finish_encoded_count(&next, &slots, encoded, bound, &self.memory);
            })?;
            let mut parts = Vec::with_capacity(slots.len());
            for (at, slot) in slots.iter().enumerate() {
                parts.push(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!("nothing finished encoded radix partition {at}")))
                })?);
            }
            let mut chunks = Vec::new();
            let mut held = encoded.held.lock().map_err(poisoned)?;
            held.clear();
            for Part { chunks: mut part, held: charge } in parts {
                chunks.append(&mut part);
                held.push(charge);
            }
            drop(held);
            return self.out.fill(chunks);
        }
        if let Some(distinct) = self.bigint_distinct.get() {
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<i64>>>> =
                (0..RADIX_PARTITIONS).map(|_| Mutex::new(None)).collect();
            let input = distinct
                .partitions
                .iter()
                .map(|partition| {
                    partition
                        .lock()
                        .map(|held| held.runs.iter().map(Vec::len).sum::<usize>())
                        .map_err(poisoned)
                })
                .sum::<Result<usize>>()?;
            let degree = degree_for(input, threads);
            together(threads, degree, &|| {
                finish_bigint_distinct(&next, &slots, distinct, &self.memory);
            })?;
            let mut total = 0_i64;
            for (at, slot) in slots.iter().enumerate() {
                let count = slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!("nothing finished distinct radix partition {at}")))
                })?;
                total = total
                    .checked_add(count)
                    .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
            }
            let mut held = distinct.held.lock().map_err(poisoned)?;
            held.clear();
            let values = [vec![Value::BigInt(total)]];
            let mut output = self.memory.reservation();
            let chunks = rows::chunks(&[LogicalType::BigInt], &values, &mut output)?;
            held.push(output);
            drop(held);
            return self.out.fill(chunks);
        }
        if let Some(fixed) = self.fixed.get() {
            let bound = self.top_counts.expect("a fixed exchange has a TopN bound").0;
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<Part>>>> =
                (0..RADIX_PARTITIONS).map(|_| Mutex::new(None)).collect();
            let degree = threads.degree().clamp(1, RADIX_PARTITIONS);
            together(threads, degree, &|| {
                finish_fixed(&next, &slots, fixed, bound, &self.calls, &self.memory);
            })?;
            let mut parts = Vec::with_capacity(slots.len());
            for (at, slot) in slots.iter().enumerate() {
                parts.push(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!("nothing finished fixed radix partition {at}")))
                })?);
            }
            let mut chunks = Vec::new();
            let mut held = fixed.held.lock().map_err(poisoned)?;
            held.clear();
            for Part { chunks: mut part, held: charge } in parts {
                chunks.append(&mut part);
                held.push(charge);
            }
            drop(held);
            return self.out.fill(chunks);
        }
        if let Some(dense) = self.dense.get() {
            let mut working = self.memory.reservation();
            working.grow(width_of(dense.dictionary.len() * size_of::<i64>()))?;
            let types = self.schema.types();
            let group_types = &types[..self.groups.len()];
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<Vec<Chunk>>>>> =
                (0..dense.partitions.len()).map(|_| Mutex::new(None)).collect();
            let degree = threads.degree().clamp(1, slots.len().max(1));
            together(threads, degree, &|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(partition) = dense.partitions.get(at) else { return };
                    let done = partition.lock().map_err(poisoned).and_then(|mut held| {
                        dense_partition(
                            &dense.dictionary,
                            at,
                            &mut held,
                            &self.constants,
                            group_types,
                        )
                    });
                    if let Ok(mut slot) = slots[at].lock() {
                        *slot = Some(done);
                    }
                }
            })?;
            let mut chunks = Vec::new();
            for (at, slot) in slots.iter().enumerate() {
                chunks.extend(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!("nothing finished dense partition {at}")))
                })?);
            }
            let mut output = self.memory.reservation();
            let shared = dense.dictionary.footprint();
            let output_bytes = chunks
                .iter()
                .map(Chunk::footprint)
                .sum::<usize>()
                .saturating_sub(shared.saturating_mul(chunks.len().saturating_sub(1)));
            output.grow(width_of(output_bytes))?;
            let mut held = dense.held.lock().map_err(poisoned)?;
            held.clear();
            working.release();
            held.push(output);
            drop(held);
            return self.out.fill(chunks);
        }
        let mut built = self.built.lock().map_err(poisoned)?;
        let degree = built.instances.min(threads.degree()).clamp(1, self.merged.len());
        let closed =
            if degree > 1 { self.close_together(threads, degree)? } else { self.close_in_turn()? };
        for part in closed {
            let Part { mut chunks, held } = part?;
            built.chunks.append(&mut chunks);
            built.held.push(held);
        }
        let chunks = std::mem::take(&mut built.chunks);
        drop(built);
        self.out.fill(chunks)
    }
}

fn finish_encoded_count(
    next: &AtomicUsize,
    slots: &[Mutex<Option<Result<Part>>>],
    encoded: &EncodedCountExchange,
    bound: usize,
    memory: &Memory,
) {
    loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(partition) = encoded.partitions.get(at) else {
            return;
        };
        let done = partition.lock().map_err(poisoned).and_then(|mut rows| {
            encoded_count_partition(&mut rows, &encoded.dictionary, &encoded.leading, bound, memory)
        });
        if let Ok(mut slot) = slots[at].lock() {
            *slot = Some(done);
        }
    }
}

/// Where `row` sits in the group table, as the slot holding it or the free bucket to open for it.
///
/// Split out because the fold reads it twice, once to compact the run it took as the table and once
/// for every other run, and those two differ only in what they do with a miss.
#[inline]
fn encoded_slot(
    buckets: &[u32],
    groups: &EncodedCountPartition,
    row: EncodedCountRecord,
    valid: u8,
) -> std::result::Result<usize, usize> {
    let mask = buckets.len() - 1;
    let all_valid = groups.validity.is_empty();
    let tag = slot_tag(u64::from(row.hash));
    let mut at = row.hash as usize & mask;
    loop {
        let bucket = buckets[at];
        let slot = bucket & SLOT_MASK;
        if slot == EMPTY_SLOT {
            return Err(at);
        }
        if bucket == tag | slot {
            let slot = slot as usize;
            let held = groups.rows[slot];
            let held_valid =
                if all_valid { EncodedCountRecord::ALL } else { groups.validity[slot] };
            if held.hash == row.hash
                && held.first == row.first
                && held.second == row.second
                && held.third == row.third
                && held_valid == valid
            {
                return Ok(slot);
            }
        }
        at = (at + 1) & mask;
    }
}

fn encoded_count_partition(
    runs: &mut EncodedCountRuns,
    dictionary: &Vector,
    leading: &[LogicalType],
    bound: usize,
    memory: &Memory,
) -> Result<Part> {
    let keys = leading.len() + 1;
    if !(2..=3).contains(&keys) {
        return Err(Error::internal("an encoded count partition has an unsupported key width"));
    }
    let reserving = stage::Timing::start(Stage::Reserve);
    let (mut partition, total) = runs.seed();
    let capacity = total.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    let room = total.saturating_sub(partition.rows.len());
    working.grow(width_of(
        capacity * size_of::<u32>()
            + total * size_of::<i64>()
            + room * size_of::<EncodedCountRecord>(),
    ))?;
    let mut buckets = vec![EMPTY_SLOT; capacity];
    let mut counts: Vec<i64> = Vec::with_capacity(total);
    // The table grows by one group per record the other runs hold that this one has not seen, and
    // reserving for all of them up front is one allocation instead of a doubling walk under a fold.
    partition.rows.reserve(room);
    reserving.stop(0);
    let timing = stage::Timing::start(Stage::Fold);
    // The run this took as the table, compacted in place: the group for a record always lands at a
    // slot at or behind where the record was read from, so nothing unread is ever written over.
    let seeded = partition.rows.len();
    let all_valid = partition.validity.is_empty();
    for source in 0..seeded {
        let row = partition.rows[source];
        let valid = if all_valid { EncodedCountRecord::ALL } else { partition.validity[source] };
        let slot = match encoded_slot(&buckets, &partition, row, valid) {
            Ok(slot) => slot,
            Err(bucket) => {
                let slot = counts.len();
                buckets[bucket] = bucket_for(
                    slot,
                    u64::from(row.hash),
                    "an encoded radix partition is too large",
                )?;
                partition.rows[slot] = row;
                if !all_valid {
                    partition.validity[slot] = valid;
                }
                counts.push(0);
                slot
            }
        };
        counts[slot] = counts[slot]
            .checked_add(1)
            .ok_or_else(|| Error::out_of_range("a grouped COUNT overflowed BIGINT"))?;
    }
    partition.rows.truncate(counts.len());
    if !all_valid {
        partition.validity.truncate(counts.len());
    }
    timing.stop(0);
    // Every other instance's run, folded into that table and given back one run at a time rather
    // than all at the end, so the records this has finished with stop costing anything.
    let timing = stage::Timing::start(Stage::Merge);
    for run in std::mem::take(&mut runs.runs) {
        let all_valid = run.validity.is_empty();
        for (source, &row) in run.rows.iter().enumerate() {
            let valid = if all_valid { EncodedCountRecord::ALL } else { run.validity[source] };
            let slot = match encoded_slot(&buckets, &partition, row, valid) {
                Ok(slot) => slot,
                Err(bucket) => {
                    let slot = counts.len();
                    buckets[bucket] = bucket_for(
                        slot,
                        u64::from(row.hash),
                        "an encoded radix partition is too large",
                    )?;
                    partition.push(row, valid);
                    counts.push(0);
                    slot
                }
            };
            counts[slot] = counts[slot]
                .checked_add(1)
                .ok_or_else(|| Error::out_of_range("a grouped COUNT overflowed BIGINT"))?;
        }
    }
    // Read again because a run past the first can have been what gave this partition its first null.
    let all_valid = partition.validity.is_empty();
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Emit);
    let mut best = Vec::with_capacity(bound.min(counts.len()));
    for slot in 0..counts.len() {
        let at = best.partition_point(|&kept| counts[kept] >= counts[slot]);
        if at < bound {
            best.insert(at, slot);
            best.truncate(bound);
        }
    }
    best.sort_unstable();
    let mut output = Vec::with_capacity(best.len());
    for slot in best {
        let key = partition.rows[slot];
        let valid = if all_valid { EncodedCountRecord::ALL } else { partition.validity[slot] };
        let first = if valid & EncodedCountRecord::FIRST != 0 {
            signed_value(&leading[0], key.first)?
        } else {
            Value::Null
        };
        let third = if valid & EncodedCountRecord::THIRD != 0 {
            dictionary.try_value_at(key.third as usize)?
        } else {
            Value::Null
        };
        let mut row = Vec::with_capacity(keys + 1);
        row.push(first);
        if keys == 3 {
            row.push(if valid & EncodedCountRecord::SECOND != 0 {
                signed_value(&leading[1], key.second)?
            } else {
                Value::Null
            });
        }
        row.push(third);
        row.push(Value::BigInt(counts[slot]));
        output.push(row);
    }
    let mut held = memory.reservation();
    let mut types = leading.to_vec();
    types.push(LogicalType::Varchar);
    types.push(LogicalType::BigInt);
    let chunks = rows::chunks(&types, &output, &mut held)?;
    timing.stop(0);
    Ok(Part { chunks, held })
}

fn finish_bigint_distinct(
    next: &AtomicUsize,
    slots: &[Mutex<Option<Result<i64>>>],
    distinct: &BigIntDistinctExchange,
    memory: &Memory,
) {
    loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(partition) = distinct.partitions.get(at) else {
            return;
        };
        let done = partition
            .lock()
            .map_err(poisoned)
            .and_then(|mut rows| bigint_distinct_partition(&mut rows, memory));
        if let Ok(mut slot) = slots[at].lock() {
            *slot = Some(done);
        }
    }
}

/// One value into the partition its hash picks, which is the top bits of the hash.
///
/// Pulled out of [`Aggregate::buffer_bigint_distinct`] so that the loop that reads a flat run of
/// words and the loop that asks the vector a row at a time cannot drift apart on which partition a
/// value belongs in or on what its hash is.
///
/// The shift leaves exactly the bits that index [`RADIX_PARTITIONS`] of them, so the index is always
/// in range and the bounds check never fires.
///
/// Only the value is kept. The hash it was placed by is four instructions to work out again and
/// eight bytes a row to carry, and on a million rows those eight bytes are written once, moved once
/// and read once, so the count that reads them pays for them three times over.
#[inline]
fn scatter_bigint(partitions: &mut [BigIntDistinctPartition], shift: u32, value: i64) {
    let hash = spread(mix(0, value as u64));
    partitions[(hash >> shift) as usize].rows.push(value);
}

/// How many distinct values one radix partition holds, across the runs its instances handed over.
///
/// The table is the values themselves with a bit a slot saying which ones are filled, rather than an
/// index into the run the way it was when there was one run to index. Nothing is moved into place, so
/// the runs are only ever read.
fn bigint_distinct_partition(partition: &mut BigIntDistinctRuns, memory: &Memory) -> Result<i64> {
    let held: usize = partition.runs.iter().map(Vec::len).sum();
    let capacity = held.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width_of(capacity * size_of::<i64>() + capacity.div_ceil(8)))?;
    let mut slots = vec![0_i64; capacity];
    let mut filled = vec![0_u64; capacity.div_ceil(64)];
    let mask = capacity - 1;
    let mut unique = 0_usize;
    let timing = stage::Timing::start(Stage::Fold);
    for run in &partition.runs {
        for &value in run {
            let mut at = spread(mix(0, value as u64)) as usize & mask;
            loop {
                let bit = 1_u64 << (at % 64);
                if filled[at / 64] & bit == 0 {
                    filled[at / 64] |= bit;
                    slots[at] = value;
                    unique += 1;
                    break;
                }
                if slots[at] == value {
                    break;
                }
                at = (at + 1) & mask;
            }
        }
    }
    timing.stop(0);
    i64::try_from(unique).map_err(|_| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))
}

fn finish_fixed(
    next: &AtomicUsize,
    slots: &[Mutex<Option<Result<Part>>>],
    fixed: &FixedExchange,
    bound: usize,
    calls: &[Call],
    memory: &Memory,
) {
    loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(partition) = fixed.partitions.get(at) else {
            return;
        };
        let done = partition
            .lock()
            .map_err(poisoned)
            .and_then(|mut rows| fixed_partition(&mut rows, &fixed.keys, bound, calls, memory));
        if let Ok(mut slot) = slots[at].lock() {
            *slot = Some(done);
        }
    }
}

/// Where `row` sits in the group table, as the slot holding it or the free bucket to open for it.
/// The same split [`encoded_slot`] is, for the fixed record's two keys.
#[inline]
fn fixed_slot(
    buckets: &[u32],
    groups: &FixedPartition,
    row: FixedRecord,
    valid: u8,
) -> std::result::Result<usize, usize> {
    const KEYS: u8 = FixedRecord::FIRST | FixedRecord::SECOND;
    let mask = buckets.len() - 1;
    let all_valid = groups.validity.is_empty();
    let hash = fixed_hash(row, valid);
    let tag = slot_tag(hash);
    let mut at = hash as usize & mask;
    loop {
        let bucket = buckets[at];
        let slot = bucket & SLOT_MASK;
        if slot == EMPTY_SLOT {
            return Err(at);
        }
        if bucket == tag | slot {
            let slot = slot as usize;
            let held = groups.rows[slot];
            let held_valid = if all_valid { FixedRecord::ALL } else { groups.validity[slot] };
            if held.first == row.first
                && held.second == row.second
                && held_valid & KEYS == valid & KEYS
            {
                return Ok(slot);
            }
        }
        at = (at + 1) & mask;
    }
}

fn fixed_partition(
    runs: &mut FixedRuns,
    keys: &[LogicalType; 2],
    bound: usize,
    calls: &[Call],
    memory: &Memory,
) -> Result<Part> {
    let reserving = stage::Timing::start(Stage::Reserve);
    let (mut partition, total) = runs.seed();
    let capacity = total.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    let room = total.saturating_sub(partition.rows.len());
    working.grow(width_of(
        capacity * size_of::<u32>()
            + total * size_of::<CompactNumeric>()
            + room * size_of::<FixedRecord>(),
    ))?;
    let mut buckets = vec![EMPTY_SLOT; capacity];
    let mut states: Vec<CompactNumeric> = Vec::with_capacity(total);
    let mut overflow = HashMap::new();
    partition.rows.reserve(room);
    reserving.stop(0);
    let timing = stage::Timing::start(Stage::Fold);
    let seeded = partition.rows.len();
    let all_valid = partition.validity.is_empty();
    for source in 0..seeded {
        let row = partition.rows[source];
        let valid = if all_valid { FixedRecord::ALL } else { partition.validity[source] };
        let slot = match fixed_slot(&buckets, &partition, row, valid) {
            Ok(slot) => slot,
            Err(bucket) => {
                let slot = states.len();
                buckets[bucket] = bucket_for(
                    slot,
                    fixed_hash(row, valid),
                    "a fixed radix partition is too large",
                )?;
                partition.rows[slot] = row;
                if !all_valid {
                    partition.validity[slot] = valid;
                }
                states.push(CompactNumeric::default());
                slot
            }
        };
        states[slot].add(
            slot,
            (valid & FixedRecord::SUM != 0).then_some(row.sum),
            (valid & FixedRecord::MEAN != 0).then_some(row.mean),
            &mut overflow,
        )?;
    }
    partition.rows.truncate(states.len());
    if !all_valid {
        partition.validity.truncate(states.len());
    }
    timing.stop(0);
    // Every other instance's run, folded into that table, which is the merge half of the close.
    let timing = stage::Timing::start(Stage::Merge);
    for run in std::mem::take(&mut runs.runs) {
        let all_valid = run.validity.is_empty();
        for (source, &row) in run.rows.iter().enumerate() {
            let valid = if all_valid { FixedRecord::ALL } else { run.validity[source] };
            let slot = match fixed_slot(&buckets, &partition, row, valid) {
                Ok(slot) => slot,
                Err(bucket) => {
                    let slot = states.len();
                    buckets[bucket] = bucket_for(
                        slot,
                        fixed_hash(row, valid),
                        "a fixed radix partition is too large",
                    )?;
                    partition.push(row, valid);
                    states.push(CompactNumeric::default());
                    slot
                }
            };
            states[slot].add(
                slot,
                (valid & FixedRecord::SUM != 0).then_some(row.sum),
                (valid & FixedRecord::MEAN != 0).then_some(row.mean),
                &mut overflow,
            )?;
        }
    }
    // Read again because a run past the first can have been what gave this partition its first null.
    let all_valid = partition.validity.is_empty();
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Emit);
    let mut best: Vec<usize> = Vec::with_capacity(bound.min(states.len()));
    for slot in 0..states.len() {
        let at = best.partition_point(|&kept| states[kept].count() >= states[slot].count());
        if at < bound {
            best.insert(at, slot);
            best.truncate(bound);
        }
    }
    best.sort_unstable();
    let mut output = Vec::with_capacity(best.len());
    for slot in best {
        let key = partition.rows[slot];
        let valid = if all_valid { FixedRecord::ALL } else { partition.validity[slot] };
        let state = &states[slot];
        let (sum, mean) = state.totals(slot, &overflow);
        output.push(vec![
            if valid & FixedRecord::FIRST != 0 {
                signed_value(&keys[0], key.first)?
            } else {
                Value::Null
            },
            if valid & FixedRecord::SECOND != 0 {
                signed_value(&keys[1], i64::from(key.second))?
            } else {
                Value::Null
            },
            Value::BigInt(state.count()),
            Accumulator::exact_sum(sum, state.sum_seen(), &calls[1].returns).finish()?,
            Accumulator::exact_avg(mean, state.mean_count, &calls[2].returns).finish()?,
        ]);
    }
    let mut held = memory.reservation();
    let types = [
        keys[0].clone(),
        keys[1].clone(),
        LogicalType::BigInt,
        calls[1].returns.clone(),
        calls[2].returns.clone(),
    ];
    let chunks = rows::chunks(&types, &output, &mut held)?;
    timing.stop(0);
    Ok(Part { chunks, held })
}

fn dense_partition(
    dictionary: &Arc<Vector>,
    number: usize,
    partition: &mut DensePartition,
    constants: &[Option<Value>],
    group_types: &[LogicalType],
) -> Result<Vec<Chunk>> {
    let width = dictionary.len().saturating_add(DENSE_PARTITIONS - 1 - number) / DENSE_PARTITIONS;
    let mut dense = vec![0_i64; width];
    for run in &partition.runs {
        for &code in run {
            dense[code as usize / DENSE_PARTITIONS] += 1;
        }
    }
    let mut chunks = Vec::new();
    let mut codes = Vec::with_capacity(VECTOR_SIZE);
    let mut counts = Vec::with_capacity(VECTOR_SIZE);
    let mut valid = Vec::with_capacity(VECTOR_SIZE);
    for (slot, &count) in dense.iter().enumerate() {
        if count == 0 {
            continue;
        }
        codes.push((slot * DENSE_PARTITIONS + number) as u32);
        counts.push(count);
        valid.push(true);
        if codes.len() == VECTOR_SIZE {
            chunks.push(dense_chunk(dictionary, &codes, &counts, &valid, constants, group_types)?);
            codes.clear();
            counts.clear();
            valid.clear();
        }
    }
    if number == 0 && partition.nulls != 0 {
        codes.push(0);
        counts.push(partition.nulls);
        valid.push(false);
    }
    if !codes.is_empty() {
        chunks.push(dense_chunk(dictionary, &codes, &counts, &valid, constants, group_types)?);
    }
    Ok(chunks)
}

fn dense_chunk(
    dictionary: &Arc<Vector>,
    codes: &[u32],
    counts: &[i64],
    valid: &[bool],
    constants: &[Option<Value>],
    group_types: &[LogicalType],
) -> Result<Chunk> {
    let validity = Validity::from_iter(valid.len(), |row| valid[row]);
    let key =
        Vector::stable_dictionary(codes.to_vec(), Arc::clone(dictionary))?.with_validity(validity);
    let mut key = Some(key);
    let mut columns = Vec::with_capacity(constants.len() + 1);
    for (constant, ty) in constants.iter().zip(group_types) {
        columns.push(match constant {
            Some(value) => Vector::constant(ty.clone(), value.clone(), codes.len()),
            None => key
                .take()
                .ok_or_else(|| Error::internal("a dense count has more than one varying key"))?,
        });
    }
    let counts = Vector::flat(LogicalType::BigInt, Data::Int64(counts.to_vec().into()))?;
    columns.push(counts);
    Chunk::with_rows(columns, codes.len())
}

/// One partition's share of the answer, and what holding it is charged.
#[derive(Debug)]
struct Part {
    chunks: Vec<Chunk>,
    held: Reservation,
}

/// How many threads to finish `input` rows of radix partitions on.
///
/// Two bounds and both of them matter. There is no point starting a thread for every partition when
/// there are only a few thousand rows between all of them, because the wake and the join cost more
/// than the rows do, and that is what the divisor says. And there is no point asking for more
/// threads than the query was given, which is what the lease says and what this used to ignore: a
/// session that set the thread count to one still finished an aggregate on sixteen.
///
/// The divisor was sixty five thousand, which on a million rows says sixteen threads whatever the
/// machine has. That was the same number the scan happened to cut morsels at, so nothing showed,
/// and now that a pipeline can borrow more threads than its source runs instances on it is what
/// would hold the finish at half the machine. Sixteen thousand is the same argument at the size a
/// woken thread is actually worth paying for.
fn degree_for(input: usize, threads: &Lease<'_>) -> usize {
    input.div_ceil(16_384).clamp(1, RADIX_PARTITIONS).min(threads.degree())
}

impl Aggregate<'_> {
    /// Every partition finished on this thread, which is what one instance means.
    fn close_in_turn(&self) -> Result<Vec<Result<Part>>> {
        Ok((0..self.merged.len()).map(|at| self.close(at)).collect())
    }

    /// Every partition finished across `degree` threads, each thread taking whichever is next.
    ///
    /// The threads are the driver's own, borrowed a second time. By the time a sink finalises the
    /// driver has already joined every instance, so the threads its lease covers are parked with
    /// nothing to do, and the close is the largest single thing left in several ClickBench queries.
    /// Starting fresh ones instead cost a thread creation apiece, which is about sixteen
    /// microseconds on the bench machine and a quarter of a millisecond for sixteen of them, paid
    /// before the first partition is looked at. That is what the pool exists to stop paying.
    ///
    /// The lease is asked for no more threads than there are partitions, because a thread handed
    /// none of them is a wake and a join spent to find out there is nothing to do.
    ///
    /// The results go into a slot apiece and are read back in partition order, so which thread got
    /// which partition and which finished first change nothing about the answer. That is what makes
    /// this safe to do at all: the rows come out in the order the one thread put them in. A thread
    /// that panics leaves its slot empty, and an empty slot is reported rather than silently
    /// dropping a partition.
    ///
    /// Each of these threads hands its stage clock back on the way out and the thread that asked
    /// adds the readings to its own, so that merging and emitting are charged to the aggregate that
    /// did them. Without it they are charged to nobody: the instrumentation shim reads the clock on
    /// the thread that called `finalize` and these are not that thread. On ClickBench at a million
    /// rows that was a third of a `GROUP BY URL` sitting in wall time with no counter anywhere to
    /// say what it was.
    fn close_together(&self, threads: &Lease<'_>, degree: usize) -> Result<Vec<Result<Part>>> {
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Result<Part>>>> =
            (0..self.merged.len()).map(|_| Mutex::new(None)).collect();
        together(threads, degree, &|| self.closing(&next, &slots))?;
        let mut closed = Vec::with_capacity(slots.len());
        for (at, slot) in slots.into_iter().enumerate() {
            closed.push(slot.into_inner().map_err(poisoned)?.unwrap_or_else(|| {
                Err(Error::internal(format!("nothing finished partition {at} of an aggregate")))
            }));
        }
        Ok(closed)
    }

    /// One thread taking partitions until there are none left.
    fn closing(&self, next: &AtomicUsize, slots: &[Mutex<Option<Result<Part>>>]) {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(slot) = slots.get(at) else { return };
            let done = self.close(at);
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(done);
            }
        }
    }

    /// One partition turned into the rows it answers for.
    ///
    /// The later passes of a spilled partition happen here. A partition's file only ever holds keys
    /// belonging to that partition, so no other partition has anything to say about them and this is
    /// the whole of finishing them.
    fn close(&self, at: usize) -> Result<Part> {
        let mut part = Part { chunks: Vec::new(), held: self.memory.reservation() };
        let mut partition = self.merged[at].lock().map_err(poisoned)?;
        let Partition { table, carried, pending } = &mut *partition;
        // The tables the instances kept to themselves, folded into one. Each of them holds only
        // keys belonging to this partition, so this is the one place they can meet, and it is one
        // probe per group rather than per row. Sixteen of these run at once, one per partition.
        let mut kept = table.take();
        for arriving in std::mem::take(pending) {
            match &mut kept {
                Some(into) => self.merge(arriving, into)?,
                None => kept = Some(arriving),
            }
        }
        let Some(kept) = kept else {
            debug_assert!(carried.is_none(), "nothing is set aside from a partition with no table");
            return Ok(part);
        };
        let Part { chunks, held } = &mut part;
        let mut left = self.finish(kept, chunks, held)?;
        // The groups set aside join the first pass that reads the file back, because that pass is
        // where their rows are. A table that opened a file and then never had a row to write to it
        // hands back no file, and then the set aside groups are whole on their own and finish as a
        // table of their own. Either way each of them is finished once, because a group is only ever
        // set aside by a partition that did not hold its key.
        if left.is_none() {
            if let Some(whole) = carried.take() {
                left = self.finish(whole, chunks, held)?;
            }
        }
        while let Some(mut file) = left {
            left = self.again(&mut file, carried.take(), chunks, held)?;
        }
        Ok(part)
    }
}

/// Whether a group key is a signed integer a fixed width radix record can hold.
///
/// The exchanges that take one widen it to eight bytes and narrow it back at the emit, so the width
/// of the column does not matter and only the signedness and the integerness do. `DATE` and
/// `TIMESTAMP` have a signed representation too and are deliberately not here, because putting one
/// back together is more than a narrowing and nothing asks for it yet.
/// Whether every null this vector has is one its own mask knows about.
///
/// [`Vector::is_null_at`] reads through a dictionary or a run to the vector standing behind it and
/// answers from the mask for every other form, so those two are where a chunk wide null check is not
/// the whole answer and a caller has to keep asking row by row.
fn nulls_are_in_the_mask(column: &Vector) -> bool {
    !matches!(column.form(), Form::Dictionary | Form::Rle)
}

/// One signed key column of a chunk with its layout settled once rather than once per row.
///
/// Asking a vector for a signed value a row at a time costs a match on the form, a match on the
/// width, a widening to a hundred and twenty eight bits and a checked narrowing back on the way out,
/// and none of that depends on the row. On the million row ClickBench file that is most of what a
/// scatter over a `BIGINT` key does. The forms here are the ones whose nulls all live in the
/// vector's own mask, so a chunk with none has none for every row of it and the row loop stops
/// asking about validity at all.
#[derive(Debug, Clone, Copy)]
enum Signed<'a> {
    Int8(&'a [i8]),
    Int16(&'a [i16]),
    Int32(&'a [i32]),
    Int64(&'a [i64]),
}

impl<'a> Signed<'a> {
    /// How `column` holds its first `rows` values, and `None` when one of them is null or the column
    /// is in a form this does not read.
    fn of(column: &'a Vector, rows: usize) -> Option<Self> {
        if column.validity().has_nulls(rows) {
            return None;
        }
        match column.data()? {
            Data::Int8(values) => values.get(..rows).map(Signed::Int8),
            Data::Int16(values) => values.get(..rows).map(Signed::Int16),
            Data::Int32(values) => values.get(..rows).map(Signed::Int32),
            Data::Int64(values) => values.get(..rows).map(Signed::Int64),
            _ => None,
        }
    }

    /// The value at `row`, which is inside the length this was built with.
    #[inline]
    fn at(self, row: usize) -> i64 {
        match self {
            Self::Int8(values) => i64::from(values[row]),
            Self::Int16(values) => i64::from(values[row]),
            Self::Int32(values) => i64::from(values[row]),
            Self::Int64(values) => values[row],
        }
    }
}

fn signed_key(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    )
}

/// Whether a group key is a signed integer that fits in the four byte half of a fixed record.
///
/// This is [`signed_key`] without `BIGINT`. The record keeps its second key four bytes wide so that
/// the whole thing stays sixteen, and a wider column there would have to be turned away rather than
/// truncated.
fn narrow_key(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer)
}

/// One signed integer key put back into the type the query asked for.
///
/// The narrowing cannot lose anything, because the value came out of a column of this type in the
/// first place and was only widened to give every key one width. It is checked rather than assumed
/// because the thing that would break it is a record reaching the wrong emit, and a wrong answer is
/// a worse way to find that out than an error is.
pub(crate) fn signed_value(ty: &LogicalType, value: i64) -> Result<Value> {
    match ty {
        LogicalType::TinyInt => i8::try_from(value).map(Value::TinyInt).map_err(|_| too_wide(ty)),
        LogicalType::SmallInt => {
            i16::try_from(value).map(Value::SmallInt).map_err(|_| too_wide(ty))
        }
        LogicalType::Integer => i32::try_from(value).map(Value::Integer).map_err(|_| too_wide(ty)),
        LogicalType::BigInt => Ok(Value::BigInt(value)),
        _ => Err(Error::internal(format!("{ty} is not a signed integer group key"))),
    }
}

fn too_wide(ty: &LogicalType) -> Error {
    Error::internal(format!("a radix group key does not fit back into {ty}"))
}

/// Charges a table for what folding groups into it grew, which is the same three sums every time.
fn charge(into: &mut Building, grown: u64) -> Result<()> {
    into.containers.grow(grown)?;
    rows::capacity(into.table.owned(), &mut into.charged_keys, &mut into.scratch)?;
    let now =
        tables(&into.table, &into.states, &into.counts, &into.compact, &into.overflow, &into.seen);
    rows::capacity(now, &mut into.charged, &mut into.containers)
}

/// The instance being folded into another, which is five things that only travel together.
struct Folding<'a> {
    count_only: bool,
    calls: usize,
    /// Which calls keep a set of the values they have accepted, so that a slot knows whether to
    /// combine two accumulators or to put two sets together.
    distinct: &'a [bool],
    taken: &'a [Accumulator],
    tallies: &'a [i64],
    compact: &'a [CompactNumeric],
    overflow: &'a HashMap<usize, (i128, i128)>,
    /// Taken by a mutable borrow because the sets are emptied as they are folded in, which is what
    /// keeps a value that moves from one set to the other from being copied.
    watched: &'a mut [DistinctSet],
}

/// Folds the aggregates of one group of one instance into the same group of another.
///
/// Free rather than a method because the caller holds a disjoint borrow of the incoming instance's
/// states and the kept instance's, and there is no way to say that from inside either of them.
///
/// A grouped `count(*)` is one integer per group and not an accumulator, which is #61, so it adds
/// rather than combining. A `DISTINCT` call is its set of accepted values put together with the kept
/// group's, folding in only what the kept set did not already have. Everything else is a run of
/// `calls` accumulators starting at the group's slot, and the two runs line up because both
/// instances were built from the same call list.
///
/// What comes back is what the kept sets took from the allocator for the values that moved into
/// them, which the caller charges once for the whole merge.
fn merge_slot(
    from: &mut Folding<'_>,
    slot: usize,
    target: usize,
    into: &mut Building,
) -> Result<u64> {
    if let Some(arriving) = from.compact.get(slot) {
        let kept = &mut into.compact[target];
        kept.combine(target, arriving, slot, from.overflow, &mut into.overflow)?;
        return Ok(0);
    }
    if from.count_only {
        into.counts[target] += from.tallies[slot];
        return Ok(0);
    }
    let calls = from.calls;
    let mut aside = 0;
    for at in 0..calls {
        if !from.distinct[at] {
            into.states[target * calls + at].combine(&from.taken[slot * calls + at])?;
            continue;
        }
        // Taken out rather than read, so that a value the kept set does not have is moved into it
        // and one it does have is dropped. Either way nothing is copied, which is the same trade
        // the fold makes when it asks a set before it adds to it.
        let arriving = std::mem::replace(
            &mut from.watched[slot * calls + at],
            DistinctSet::Row(RowSet::default()),
        );
        let state = &mut into.states[target * calls + at];
        match (&mut into.seen[target * calls + at], arriving) {
            (DistinctSet::BigInt(kept), DistinctSet::BigInt(arriving)) => {
                arriving.into_each(|value| {
                    if kept.insert(value) {
                        aside += width_of(size_of::<i64>() * 2);
                        state.update(&[Value::BigInt(value)])?;
                    }
                    Ok(())
                })?;
            }
            (DistinctSet::Row(kept), DistinctSet::Row(arriving)) => {
                for key in arriving {
                    if kept.contains(&key) {
                        continue;
                    }
                    state.update(&key.0)?;
                    aside += rows::footprint(&key.0);
                    kept.insert(key);
                }
            }
            _ => {
                return Err(Error::internal(
                    "two instances of one DISTINCT aggregate disagree about what their sets hold",
                ));
            }
        }
    }
    Ok(aside)
}

/// What the three containers have taken from the allocator between them.
///
/// Capacity rather than length in all three, which is the point of #227. A `Vec` doubles and so sits
/// between half empty and full, so a table of seventeen million groups has paid for somewhere
/// between seventeen and thirty four million slots and the old charge counted seventeen.
/// [`Table::footprint`] has the same arithmetic over the three parts a group table is made of.
///
/// What the keys own away from the table is not counted here. That is [`Table::owned`], charged
/// against the scratch instead, and the two have to divide the group between them without
/// overlapping.
///
/// An accumulator is charged as its own width and not as what it holds. That is a knowing undercount
/// and it is the one left: what a `list()` or a `string_agg()` holds grows with the input and there
/// is no way to ask one how large it has become.
fn tables(
    table: &Table,
    states: &Vec<Accumulator>,
    counts: &Vec<i64>,
    compact: &Vec<CompactNumeric>,
    overflow: &HashMap<usize, (i128, i128)>,
    seen: &Vec<DistinctSet>,
) -> u64 {
    let width = |count: usize, size: usize| {
        u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(width_of(size))
    };
    table.footprint()
        + width(states.capacity(), size_of::<Accumulator>())
        + width(counts.capacity(), size_of::<i64>())
        + width(compact.capacity(), size_of::<CompactNumeric>())
        + overflow_footprint(overflow)
        + width(seen.capacity(), size_of::<DistinctSet>())
}

fn overflow_footprint(overflow: &HashMap<usize, (i128, i128)>) -> u64 {
    width_of(overflow.capacity() * size_of::<(usize, (i128, i128))>() * 2)
}

/// A `size_of` in the width the budget is counted in.
fn width_of(size: usize) -> u64 {
    u64::try_from(size).unwrap_or(u64::MAX)
}

/// What a group column is called in this operator's schema.
///
/// A group over a plain column keeps that column's name, because the thing somebody reading a plan
/// dump or a mid-pipeline schema wants to know is which column it is. Anything else gets a
/// positional name, since the projection above an aggregate is what names the query's output and
/// these names never reach a result set.
fn group_name(plan: &Plan, group: ExprRef, input: &Schema, at: usize) -> String {
    if let Expr::Column(binding) = *plan.expr(group) {
        if let Some(position) = input.position_of(binding) {
            return input.fields()[position].name.clone();
        }
    }
    format!("group{at}")
}

/// Duplicate elimination over the whole row or over named expressions.
///
/// `DISTINCT ON (a) b` keeps the first row of each `a`, whole, which is why the kept rows are the
/// input's columns and not the key's. Plain `DISTINCT` is the same operator with the key being
/// every column, and writing it that way rather than as a separate path is what keeps the two from
/// disagreeing about nulls.
///
/// # Deduplicating twice
///
/// As a [`Sink`], an instance holds a table of what it has seen and the rows it decided to keep.
/// Two instances that both saw a row both kept it, because neither can see the other's table
/// without a lock in the row loop, so `combine` asks the same question again against one table and
/// drops what is already there. That is the standard shape for a distinct in a parallel engine and
/// the reason the instance keeps the key beside the row: the second pass needs it and rebuilding it
/// would mean evaluating the expressions again.
///
/// On one thread there is one instance, the second pass finds nothing, and the work is what it was.
#[derive(Debug)]
pub(crate) struct Distinct {
    /// The expressions `DISTINCT ON` names, empty for a plain `DISTINCT`.
    on: Prepared,
    /// Whether the key is the whole row, which is what a plain `DISTINCT` is.
    whole: bool,
    /// The input's types, which are also the output's, since a distinct drops rows and not columns.
    types: Vec<LogicalType>,
    memory: Memory,
    /// What every instance has combined into.
    global: Mutex<Held>,
    /// What the kept rows are charged, given back once the chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the kept chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// The one table and the one list of rows that survive, and what they are charged.
#[derive(Debug)]
struct Held {
    seen: RowSet,
    kept: Vec<Vec<Value>>,
    /// What the table has been charged for the room it took.
    counted: u64,
    table: Reservation,
    /// What the list of kept rows has been charged for the room it took.
    counted_rows: u64,
    slots: Reservation,
}

/// What one instance of a distinct holds while it runs.
#[derive(Debug)]
pub(crate) struct Keeping {
    seen: RowSet,
    /// The rows this instance kept, with the key each of them was kept for.
    kept: Vec<(Key, Vec<Value>)>,
    scratch: Scratch,
    /// The buffer the key of the row being looked at is built in, reused per row.
    key: Key,
    rows: Reservation,
    table: Reservation,
    counted: u64,
    counted_table: u64,
}

impl Distinct {
    /// Applies the session semantics to the distinct expressions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.on = self.on.in_session(session);
        self
    }

    /// The sink, and the source its rows come out of.
    ///
    /// # Errors
    ///
    /// If an expression in `on` does not resolve against the input's schema.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        on: Slice,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        let on = plan.expr_list(on).to_vec();
        let out = Buffered::new();
        let distinct = Self {
            whole: on.is_empty(),
            on: Prepared::new(plan, &on, input)?,
            types: input.types(),
            memory: memory.clone(),
            global: Mutex::new(Held {
                seen: RowSet::default(),
                kept: Vec::new(),
                counted: 0,
                // The table, which is gone before the chunks are built, unlike the rows it decided
                // to keep. Per #272, the same split the aggregate above makes and for the same
                // reason.
                table: memory.reservation(),
                counted_rows: 0,
                slots: memory.reservation(),
            }),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Ok((distinct, out))
    }
}

impl Sink for Distinct {
    type Local = Keeping;

    /// Only a plain `DISTINCT`, where the key is the whole row.
    ///
    /// `DISTINCT ON (a) b` keeps the first row of each `a`, and with two instances the first row of
    /// a key is whichever instance got there first. SQL does not say which row that is, so neither
    /// answer is wrong, but it would change from run to run on the same data, and an engine that
    /// does that gets a bug report. A plain `DISTINCT` has no such choice to make: the key is the
    /// whole row, so the rows that survive are the same rows whoever kept them.
    fn parallel(&self) -> bool {
        self.whole
    }

    fn local(&self) -> Keeping {
        Keeping {
            seen: RowSet::default(),
            kept: Vec::new(),
            scratch: self.on.scratch(),
            key: Key(Vec::new()),
            rows: self.memory.reservation(),
            table: self.memory.reservation(),
            counted: 0,
            counted_table: 0,
        }
    }

    fn sink(&self, chunk: &Chunk, local: &mut Keeping) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.on.len());
        self.on.evaluate(chunk, &mut local.scratch, &mut keys)?;
        let mut taken = 0;
        let mut aside = 0;
        // row at a time: `DISTINCT` is a grouping that keeps no aggregate, so it gets its answer
        // from the same table 2f (#60) builds and stops building a key here then.
        for row in 0..chunk.len() {
            if self.whole {
                local.key.0.clear();
                local.key.0 = (0..chunk.width())
                    .map(|column| chunk.try_value_at(row, column))
                    .collect::<Result<_>>()?;
            } else {
                fill(&mut local.key, &keys, row)?;
            }
            // Asked before anything is copied, because a row that has been seen is a row this has
            // no further use for, and most rows of a `DISTINCT` worth running have been.
            if local.seen.contains(&local.key) {
                continue;
            }
            let values: Vec<Value> = if self.whole {
                local.key.0.clone()
            } else {
                (0..chunk.width())
                    .map(|column| chunk.try_value_at(row, column))
                    .collect::<Result<_>>()?
            };
            // The row is kept twice, once as the key in the table and once in the output, and each
            // copy is its own block. What the table and the output took to have room for them is
            // charged below, once per chunk. The copy is charged and not the buffer it came from,
            // for the reason the group key in `build` above gives.
            let stored = local.key.clone();
            taken += rows::heap(&values) + rows::heap(&stored.0);
            aside += rows::heap(&stored.0);
            local.seen.insert(stored.clone());
            local.kept.push((stored, values));
        }
        local.rows.grow(taken)?;
        local.table.grow(aside)?;
        let held = width_of(local.kept.capacity() * size_of::<(Key, Vec<Value>)>());
        rows::capacity(held, &mut local.counted, &mut local.rows)?;
        let now = rows::buckets(local.seen.capacity()) * (width_of(size_of::<Key>()) + 1);
        rows::capacity(now, &mut local.counted_table, &mut local.table)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Keeping) -> Result<()> {
        let Keeping { seen, kept, rows, mut table, .. } = local;
        // The instance's table has answered its last question, and the one below is about to be
        // asked the same one, so it goes now rather than being held until the operator is dropped.
        drop(seen);
        table.release();
        let mut global = self.global.lock().map_err(poisoned)?;
        let global = &mut *global;
        let mut aside = 0;
        for (key, values) in kept {
            let cost = rows::heap(&key.0);
            if global.seen.insert(key) {
                aside += cost;
                global.kept.push(values);
            }
        }
        global.table.grow(aside)?;
        let now = rows::buckets(global.seen.capacity()) * (width_of(size_of::<Key>()) + 1);
        rows::capacity(now, &mut global.counted, &mut global.table)?;
        let held = width_of(global.kept.capacity() * size_of::<Vec<Value>>());
        rows::capacity(held, &mut global.counted_rows, &mut global.slots)?;
        self.charged.lock().map_err(poisoned)?.push(rows);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let mut global = self.global.lock().map_err(poisoned)?;
        let kept = std::mem::take(&mut global.kept);
        // The table is not needed to build the chunks and the rows are, so it goes first and its
        // charge goes with it, which is the room the chunks are built in.
        global.seen = RowSet::default();
        global.counted = 0;
        global.table.release();
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &kept, &mut held)?;
        self.out.fill(chunks)?;
        global.counted_rows = 0;
        global.slots.release();
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a distinct is keeping")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use rudb_common::{Field, LogicalType, Memory, Value};
    use rudb_pipeline::Sink;
    use rudb_plan::{Plan, Slice};
    use rudb_vector::{Chunk, Data, Vector};

    use super::{
        Aggregate, BigIntDistinct, BigIntDistinctRuns, Call, CompactNumeric, Distinct,
        EncodedCountPartition, EncodedCountRecord, EncodedCountRuns, FixedPartition, FixedRecord,
        FixedRuns, Signed, bigint_distinct_partition, encoded_count_partition, fixed_partition,
    };
    use crate::buffer::Buffered;
    use crate::schema::Schema;

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    /// A plain `DISTINCT` over one integer column, which is the whole row case.
    fn distinct() -> (Distinct, Buffered) {
        let schema = Schema::numbered(vec![Field::new("a", LogicalType::Integer)], 0);
        Distinct::new(&Plan::new(), &schema, Slice::EMPTY, &Memory::unlimited())
            .expect("there are no expressions to resolve")
    }

    fn column(out: &Buffered) -> Vec<Value> {
        let chunk = out.at(0).expect("readable").expect("one chunk");
        (0..chunk.len()).map(|row| chunk.value_at(row, 0)).collect()
    }

    #[test]
    fn one_instance_keeps_the_first_of_each_row() {
        let (distinct, out) = distinct();
        let mut local = distinct.local();
        distinct.sink(&chunk(&[1, 2, 1, 3, 2]), &mut local).expect("five rows");
        distinct.combine(local).expect("the one instance");
        distinct.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(column(&out), [Value::Integer(1), Value::Integer(2), Value::Integer(3)]);
    }

    /// The point of the second pass. Neither instance can see the other's table while it runs, so
    /// both of them keep the row they share, and `combine` is what makes it one row again.
    #[test]
    fn two_instances_that_both_kept_a_row_keep_one_of_it_between_them() {
        let (distinct, out) = distinct();
        let mut left = distinct.local();
        let mut right = distinct.local();
        distinct.sink(&chunk(&[1, 2]), &mut left).expect("two rows");
        distinct.sink(&chunk(&[2, 3]), &mut right).expect("two rows");
        distinct.combine(left).expect("the first instance");
        distinct.combine(right).expect("the second instance");
        distinct.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(column(&out), [Value::Integer(1), Value::Integer(2), Value::Integer(3)]);
    }

    /// An ungrouped aggregate with no calls at all, which is the smallest one there is and enough
    /// to drive the sink with.
    #[test]
    fn an_ungrouped_aggregate_answers_one_row_from_one_instance() {
        let plan = Plan::new();
        let schema = Schema::numbered(vec![Field::new("a", LogicalType::Integer)], 0);
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, Slice::EMPTY, Slice::EMPTY, &Memory::unlimited())
                .expect("no aggregates to take apart");

        aggregate.combine(aggregate.local()).expect("the one instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(out.at(0).expect("readable").expect("one chunk").len(), 1);
    }

    /// A plan holding one aggregate over one integer column, parsed from its textual form because
    /// that is three lines instead of thirty of arena building and because it is the notation a plan
    /// dump already uses.
    fn parsed(text: &str) -> Plan {
        Plan::parse(&format!("{text}\n  Get memory.main.t AS t #0 [x::INTEGER]"))
            .expect("a plan this crate's own notation describes")
    }

    /// The aggregate at the root of such a plan, wired to a buffer to answer into.
    fn aggregate(plan: &Plan) -> (Aggregate<'_>, Buffered) {
        let schema = Schema::numbered(vec![Field::new("x", LogicalType::Integer)], 0);
        let (groups, aggregates) = match *plan.node(plan.root()) {
            rudb_plan::Node::Aggregate { groups, aggregates, .. } => (groups, aggregates),
            ref other => panic!("the plan's root is {other:?} and not an aggregate"),
        };
        Aggregate::new(plan, &schema, 1, groups, aggregates, &Memory::unlimited())
            .expect("the aggregates are ones this crate implements")
    }

    /// The rows an aggregate answered, as values, in the order it produced them.
    fn answer(out: &Buffered) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for at in 0.. {
            let Some(chunk) = out.at(at).expect("readable") else { break };
            // row at a time: reading a handful of answer rows back out in a test, where a kernel
            // would be more code than the thing it checks.
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows
    }

    fn bigint_chunk(values: &[Value]) -> Chunk {
        let column = Vector::from_values(LogicalType::BigInt, values).expect("BIGINT values");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    #[test]
    fn radix_bigint_distinct_counts_across_instances_and_skips_nulls() {
        let plan = Plan::parse(concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::BIGINT)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [x::BIGINT]",
        ))
        .expect("a distinct count plan");
        let schema = Schema::numbered(vec![Field::new("x", LogicalType::BigInt)], 0);
        let rudb_plan::Node::Aggregate { groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, groups, aggregates, &Memory::unlimited())
                .expect("a distinct count aggregate");
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate
            .sink(&bigint_chunk(&[Value::BigInt(7), Value::Null, Value::BigInt(8)]), &mut left)
            .expect("the left values");
        aggregate
            .sink(&bigint_chunk(&[Value::BigInt(8), Value::BigInt(9), Value::Null]), &mut right)
            .expect("the right values");
        aggregate.combine(left).expect("the left instance");
        aggregate.combine(right).expect("the right instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the distinct count");
        assert_eq!(answer(&out), [vec![Value::BigInt(3)]]);
    }

    #[test]
    fn radix_bigint_distinct_answers_zero_without_input() {
        let plan = Plan::parse(concat!(
            "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::BIGINT)::BIGINT]\n",
            "  Get memory.main.t AS t #0 [x::BIGINT]",
        ))
        .expect("a distinct count plan");
        let schema = Schema::numbered(vec![Field::new("x", LogicalType::BigInt)], 0);
        let rudb_plan::Node::Aggregate { groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, groups, aggregates, &Memory::unlimited())
                .expect("a distinct count aggregate");
        aggregate.combine(aggregate.local()).expect("an empty instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the zero count");
        assert_eq!(answer(&out), [vec![Value::BigInt(0)]]);
    }

    /// The point of the whole thing. Two instances see different rows of the same group, and what
    /// comes out is one row for that group with both instances' rows counted in it.
    #[test]
    fn two_instances_of_a_grouped_aggregate_answer_one_row_a_group() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate.sink(&chunk(&[1, 2, 1]), &mut left).expect("three rows");
        aggregate.sink(&chunk(&[2, 3, 2]), &mut right).expect("three rows");
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [
                vec![Value::Integer(1), Value::BigInt(2)],
                vec![Value::Integer(2), Value::BigInt(3)],
                vec![Value::Integer(3), Value::BigInt(1)],
            ]
        );
    }

    /// What used to make a pushed down limit refuse a second instance.
    ///
    /// Two instances, a limit of two, and the rows arranged so that each of them would fill its own
    /// table with different groups if they were left to choose. The left sees 1 and 2 first and the
    /// right sees 3 and 4 first, and group 1 has a row on both sides. Whichever two groups come out,
    /// their counts have to be the whole count of those groups and not one instance's share of it.
    #[test]
    fn two_instances_under_a_limit_keep_the_same_groups() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let aggregate = aggregate.limit_groups(2);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate.sink(&chunk(&[1, 2, 1, 2]), &mut left).expect("the left rows");
        aggregate.sink(&chunk(&[3, 4, 1, 2]), &mut right).expect("the right rows");
        aggregate.combine(left).expect("the left instance");
        aggregate.combine(right).expect("the right instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [vec![Value::Integer(1), Value::BigInt(3)], vec![Value::Integer(2), Value::BigInt(3)],]
        );
    }

    /// The other side of it, where the input never has as many groups as the limit asks for.
    ///
    /// Nothing is ever settled, so nothing is ever dropped, and the two instances simply open their
    /// own keys and merge. Every group has to come out with its whole count.
    #[test]
    fn two_instances_under_a_limit_nothing_reaches_keep_every_group() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let aggregate = aggregate.limit_groups(10);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate.sink(&chunk(&[1, 2]), &mut left).expect("the left rows");
        aggregate.sink(&chunk(&[2, 3]), &mut right).expect("the right rows");
        aggregate.combine(left).expect("the left instance");
        aggregate.combine(right).expect("the right instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [
                vec![Value::Integer(1), Value::BigInt(1)],
                vec![Value::Integer(2), Value::BigInt(2)],
                vec![Value::Integer(3), Value::BigInt(1)],
            ]
        );
    }

    /// The partitioned path, finished on more than one thread, which is what a large group by takes.
    ///
    /// Five thousand groups is past [`PARTITION_FROM`], so the instances hand their tables to the
    /// partitions and `finalize` finishes those partitions in parallel. What this pins is that every
    /// group comes out exactly once. A partition finished twice doubles its counts and one nobody
    /// finished loses its groups, and neither can happen on a table small enough to stay in one
    /// piece, which is every other test in here.
    #[test]
    fn a_partitioned_aggregate_answers_every_group_once() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..5_000).collect();
        for part in values.chunks(1_024) {
            aggregate.sink(&chunk(part), &mut left).expect("a chunk of groups");
            aggregate.sink(&chunk(part), &mut right).expect("the same groups again");
        }
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        let built = aggregate.built.lock().expect("readable");
        assert!(
            built.partitioning,
            "five thousand groups on two instances is meant to take the partitioned path"
        );
        assert!(built.local, "five thousand groups fit in the cache twice over");
        drop(built);
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut seen: Vec<i32> = Vec::new();
        for row in answer(&out) {
            assert_eq!(row[1], Value::BigInt(2), "{row:?} was counted on both instances");
            match row[0] {
                Value::Integer(key) => seen.push(key),
                ref other => panic!("the group is {other:?} and not an integer"),
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, values);
    }

    /// An aggregate whose private tables outgrow the cache gives them up and still answers once.
    ///
    /// The budget is unlimited here, so neither [`Aggregate::worth_local`] nor
    /// [`Aggregate::room_for_local`] has anything to object to. What ends local mode is the three
    /// cache questions together, and this arranges for all three to say so. Three calls a group
    /// over a hundred and twenty thousand groups on two instances is past [`LOCAL_CACHE`]. Each
    /// chunk goes in twice, so a group has two rows and [`copies_overlap`] says the instances are
    /// holding the same groups as each other. The chunks themselves are runs of distinct keys, so
    /// [`keys_arrive_together`] says the input has not divided the keys between the instances
    /// already.
    ///
    /// The handover is the part worth pinning. Some of each instance's rows went into a private
    /// table and the rest went straight into a shared one, and the two halves have to meet exactly
    /// once: a group counted in both is a group whose private table was folded in twice, and a
    /// group missing entirely is one whose table was dropped on the way over.
    #[test]
    fn an_aggregate_too_large_for_the_cache_gives_up_its_own_tables_and_still_answers_once() {
        let plan = parsed(concat!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT, ",
            "sum(#0.0::INTEGER)::BIGINT, min(#0.0::INTEGER)::INTEGER]"
        ));
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..120_000).collect();
        for part in values.chunks(1_024) {
            for instance in [&mut left, &mut right] {
                aggregate.sink(&chunk(part), instance).expect("a chunk of groups");
                aggregate.sink(&chunk(part), instance).expect("the same chunk a second time");
            }
        }
        assert!(
            !aggregate.built.lock().expect("readable").local,
            "a hundred and twenty thousand groups of three calls is past what the cache holds"
        );
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut seen: Vec<i32> = Vec::new();
        for row in answer(&out) {
            let Value::Integer(key) = row[0] else {
                panic!("the group is {:?} and not an integer", row[0]);
            };
            assert_eq!(row[1], Value::BigInt(4), "{row:?} was not counted four times");
            assert_eq!(
                row[2],
                Value::BigInt(i64::from(key) * 4),
                "{row:?} was not summed four times"
            );
            assert_eq!(row[3], Value::Integer(key), "{row:?} kept the wrong least value");
            seen.push(key);
        }
        seen.sort_unstable();
        assert_eq!(seen, values);
    }

    /// Tables the input has already divided between the instances are left where they are.
    ///
    /// The same size and the same shape as the test above, and the only thing that changes is the
    /// order the keys arrive in: each group's four rows are next to each other rather than a chunk
    /// apart. That is what TPC-H lineitem looks like grouped by order key, and it means no two
    /// instances are holding the same group, so folding the tables into shared ones would take the
    /// locks and remove nothing. [`keys_arrive_together`] is what notices, and the answer still has
    /// to be right either way.
    #[test]
    fn an_aggregate_whose_keys_arrive_in_runs_keeps_its_own_tables() {
        let plan = parsed(concat!(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT, ",
            "sum(#0.0::INTEGER)::BIGINT, min(#0.0::INTEGER)::INTEGER]"
        ));
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..120_000).collect();
        let runs: Vec<i32> = values.iter().flat_map(|&key| [key, key]).collect();
        for part in runs.chunks(1_024) {
            for instance in [&mut left, &mut right] {
                aggregate.sink(&chunk(part), instance).expect("a chunk of runs");
            }
        }
        assert!(
            aggregate.built.lock().expect("readable").local,
            "keys that arrive in runs are already divided between the instances"
        );
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut seen: Vec<i32> = Vec::new();
        for row in answer(&out) {
            let Value::Integer(key) = row[0] else {
                panic!("the group is {:?} and not an integer", row[0]);
            };
            assert_eq!(row[1], Value::BigInt(4), "{row:?} was not counted four times");
            seen.push(key);
        }
        seen.sort_unstable();
        assert_eq!(seen, values);
    }

    /// The same five thousand groups, under a pushed down bound, stay in one table.
    ///
    /// The bound is applied when a table is finished, so a split applies it once per partition and
    /// lets through that many times as many rows as one table would. With five thousand groups
    /// spread over sixty four partitions not one of them reaches a bound of a thousand, so the
    /// bound stops doing anything at all and the pipeline above gets every group instead of a
    /// thousand of them. Splitting is only worth it once a partition would still hold more groups
    /// than the bound, and that is what the threshold asks, so this table stays whole and the bound
    /// bites.
    #[test]
    fn an_aggregate_under_a_pushed_down_bound_keeps_its_table_in_one_piece() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let aggregate = aggregate.top_counts(1_000, 0);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..5_000).collect();
        for part in values.chunks(1_024) {
            aggregate.sink(&chunk(part), &mut left).expect("a chunk of groups");
            aggregate.sink(&chunk(part), &mut right).expect("the same groups again");
        }
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        assert!(
            !aggregate.built.lock().expect("readable").partitioning,
            "sixty four partitions of eighty groups would let the bound through untouched"
        );
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(
            answer(&out).len(),
            1_000,
            "the bound is applied once and not once per partition"
        );
    }

    /// An ungrouped aggregate has no key to probe, so the merge is the accumulators on their own and
    /// it is worth its own test that it takes that path and gets the same total.
    #[test]
    fn two_instances_of_an_ungrouped_aggregate_add_up_to_one_total() {
        let plan = parsed(
            "Aggregate #1 groups=[] aggregates=[sum(#0.0::INTEGER)::HUGEINT, min(#0.0::INTEGER)::INTEGER, max(#0.0::INTEGER)::INTEGER, count_star()::BIGINT]",
        );
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate.sink(&chunk(&[4, 7]), &mut left).expect("two rows");
        aggregate.sink(&chunk(&[2, 9]), &mut right).expect("two rows");
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(
            answer(&out),
            [vec![Value::HugeInt(22), Value::Integer(2), Value::Integer(9), Value::BigInt(4)]]
        );
    }

    /// An instance that took no morsel still combines, and an empty table folded into a full one has
    /// to leave the full one alone rather than answering nothing or answering twice.
    #[test]
    fn an_instance_that_saw_no_rows_changes_nothing_when_it_combines() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let mut seen = aggregate.local();
        aggregate.sink(&chunk(&[5, 5]), &mut seen).expect("two rows");
        aggregate.combine(aggregate.local()).expect("an instance that saw nothing");
        aggregate.combine(seen).expect("the one that saw something");
        aggregate.combine(aggregate.local()).expect("another that saw nothing");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(answer(&out), [vec![Value::Integer(5), Value::BigInt(2)]]);
    }

    /// Two instances of a `DISTINCT` aggregate where the same value reached both of them. Adding the
    /// two counts would answer two, and the union answers one, which is what the query asked.
    #[test]
    fn two_instances_of_a_distinct_aggregate_count_a_shared_value_once() {
        let plan = parsed(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT]",
        );
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        aggregate.sink(&chunk(&[3, 3]), &mut left).expect("two rows of one group");
        aggregate.sink(&chunk(&[3, 4]), &mut right).expect("the same group and another");
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [vec![Value::Integer(3), Value::BigInt(1)], vec![Value::Integer(4), Value::BigInt(1)]]
        );
    }

    #[test]
    fn a_distinct_over_nothing_produces_nothing() {
        let (distinct, out) = distinct();
        distinct.combine(distinct.local()).expect("an instance that saw no chunks");
        distinct.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(out.len().expect("readable"), 0);
    }

    #[test]
    fn compact_smallint_totals_remain_exact_past_i64() {
        let mut wide = CompactNumeric { sum: i64::MAX, mean: i64::MIN, ..Default::default() };
        let mut wide_overflow = HashMap::new();
        wide.add(0, Some(1), Some(-1), &mut wide_overflow).expect("wide totals");
        assert_eq!(
            wide.totals(0, &wide_overflow),
            (i128::from(i64::MAX) + 1, i128::from(i64::MIN) - 1)
        );

        let mut coming = CompactNumeric::default();
        let mut coming_overflow = HashMap::new();
        coming.add(0, Some(2), Some(3), &mut coming_overflow).expect("small totals");
        wide.combine(0, &coming, 0, &coming_overflow, &mut wide_overflow).expect("combined totals");
        assert_eq!(
            wide.totals(0, &wide_overflow),
            (i128::from(i64::MAX) + 3, i128::from(i64::MIN) + 2)
        );
        assert_eq!(wide.count(), 2);
        assert_eq!(wide.mean_count, 2);
        assert_eq!(size_of::<CompactNumeric>(), 32);
    }

    /// The narrow path skips the map by asking its length, so the case that has to be checked is a
    /// map with something already in it. One group over sixty four bits and a second one under it,
    /// sharing the map, and both totals still come back exact.
    #[test]
    fn a_group_that_fits_stays_out_of_a_map_another_group_has_already_used() {
        let mut overflow = HashMap::new();
        let mut wide = CompactNumeric { sum: i64::MAX, ..Default::default() };
        wide.add(4, Some(1), None, &mut overflow).expect("wide totals");
        assert_eq!(overflow.len(), 1, "the wide group is the only one in the map");

        let mut narrow = CompactNumeric { sum: 10, ..Default::default() };
        narrow.add(9, Some(5), None, &mut overflow).expect("small totals");
        assert_eq!(overflow.len(), 1, "a group that fits sixty four bits is not written down");
        assert_eq!(narrow.totals(9, &overflow), (15, 0));
        assert_eq!(wide.totals(4, &overflow), (i128::from(i64::MAX) + 1, 0));

        // And once the wide group comes back inside, the map empties and the fast path returns.
        let zero = CompactNumeric::default();
        let mut coming = HashMap::new();
        wide.combine(4, &zero, 0, &coming, &mut overflow).expect("no change to the total");
        assert_eq!(wide.totals(4, &overflow), (i128::from(i64::MAX) + 1, 0));
        let mut back = CompactNumeric { sum: i64::MIN + 1, ..Default::default() };
        back.add(0, None, None, &mut coming).expect("a total that fits");
        wide.combine(4, &back, 0, &coming, &mut overflow).expect("back under the limit");
        assert!(overflow.is_empty(), "the group left the map when its total fitted again");
        assert_eq!(wide.totals(4, &overflow), (i128::from(i64::MAX) + i128::from(i64::MIN) + 2, 0));
    }

    #[test]
    fn a_bigint_distinct_set_allocates_only_after_its_first_value() {
        let mut values = BigIntDistinct::default();
        assert!(values.insert(7));
        assert!(!values.insert(7));
        assert!(matches!(values, BigIntDistinct::One(7)));
        assert!(values.insert(9));
        assert!(!values.insert(7));
        assert!(!values.insert(9));
        assert!(matches!(values, BigIntDistinct::Many(_)));
    }

    #[test]
    fn a_bigint_radix_partition_counts_unique_values_across_the_runs_it_was_handed() {
        let mut partition =
            BigIntDistinctRuns { runs: vec![vec![11, 12, 11], vec![13, 11], Vec::new(), vec![12]] };
        assert_eq!(
            bigint_distinct_partition(&mut partition, &Memory::unlimited())
                .expect("the distinct partition"),
            3,
            "a value counts once however many instances handed it over"
        );
    }

    #[test]
    fn a_bigint_radix_partition_tells_apart_values_that_probe_past_each_other() {
        // Enough values to fill the table past the point where a probe walks, which is what checks
        // that a slot is compared by the value in it and not only by being taken.
        let mut partition = BigIntDistinctRuns {
            runs: vec![(0..300).map(i64::from).collect(), (150..450).map(i64::from).collect()],
        };
        assert_eq!(
            bigint_distinct_partition(&mut partition, &Memory::unlimited())
                .expect("the distinct partition"),
            450,
            "four hundred and fifty different values are four hundred and fifty groups"
        );
    }

    #[test]
    fn an_encoded_count_partition_aggregates_collisions_and_nulls_exactly() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        let row = |first, second, third| EncodedCountRecord { first, second, hash: 7, third };
        let mut partition = EncodedCountPartition::default();
        partition.push(row(1, 2, 0), EncodedCountRecord::ALL);
        partition.push(row(1, 2, 0), EncodedCountRecord::ALL);
        partition.push(row(1, 2, 1), EncodedCountRecord::ALL);
        partition.push(row(0, 2, 0), EncodedCountRecord::SECOND | EncodedCountRecord::THIRD);
        let leading = [LogicalType::BigInt, LogicalType::BigInt];
        let part = encoded_count_partition(
            &mut EncodedCountRuns { runs: vec![partition] },
            &dictionary,
            &leading,
            10,
            &Memory::unlimited(),
        )
        .expect("the encoded partition");
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for chunk in part.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row| format!("{row:?}"));
        let mut expected = vec![
            vec![
                Value::BigInt(1),
                Value::BigInt(2),
                Value::Varchar("one".into()),
                Value::BigInt(2),
            ],
            vec![
                Value::BigInt(1),
                Value::BigInt(2),
                Value::Varchar("two".into()),
                Value::BigInt(1),
            ],
            vec![Value::Null, Value::BigInt(2), Value::Varchar("one".into()), Value::BigInt(1)],
        ];
        expected.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(rows, expected);
        assert_eq!(size_of::<EncodedCountRecord>(), 24);
    }

    #[test]
    fn an_encoded_count_partition_folds_every_run_into_the_widest_one() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        let row = |first, third| EncodedCountRecord { first, second: 0, hash: 7, third };
        let mut narrow = EncodedCountPartition::default();
        narrow.push(row(1, 0), EncodedCountRecord::ALL);
        let mut widest = EncodedCountPartition::default();
        for _ in 0..3 {
            widest.push(row(1, 0), EncodedCountRecord::ALL);
        }
        widest.push(row(2, 1), EncodedCountRecord::ALL);
        // The run that carries the only null is not the one the fold takes as its table, so the
        // table starts out with no validity at all and has to grow one when this arrives.
        let mut late = EncodedCountPartition::default();
        late.push(row(0, 0), EncodedCountRecord::SECOND | EncodedCountRecord::THIRD);
        late.push(row(1, 0), EncodedCountRecord::ALL);
        let leading = [LogicalType::BigInt];
        let part = encoded_count_partition(
            &mut EncodedCountRuns { runs: vec![narrow, widest, late] },
            &dictionary,
            &leading,
            10,
            &Memory::unlimited(),
        )
        .expect("the encoded partition");
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for chunk in part.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row| format!("{row:?}"));
        let mut expected = vec![
            vec![Value::BigInt(1), Value::Varchar("one".into()), Value::BigInt(5)],
            vec![Value::BigInt(2), Value::Varchar("two".into()), Value::BigInt(1)],
            vec![Value::Null, Value::Varchar("one".into()), Value::BigInt(1)],
        ];
        expected.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(rows, expected, "a group is one group however many runs it arrived in");
    }

    #[test]
    fn a_two_key_encoded_count_omits_the_unused_integer_and_narrows_the_one_it_keeps() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        let row = |first, third| EncodedCountRecord { first, second: 0, hash: 7, third };
        let mut partition = EncodedCountPartition::default();
        partition.push(row(1, 0), EncodedCountRecord::ALL);
        partition.push(row(1, 0), EncodedCountRecord::ALL);
        partition.push(row(1, 1), EncodedCountRecord::ALL);
        partition.push(row(0, 0), EncodedCountRecord::SECOND | EncodedCountRecord::THIRD);
        // A `SMALLINT` leading key, which is q14's shape. The record held it as eight bytes and the
        // emit has to hand it back two bytes wide or the answer has the wrong column type in it.
        let leading = [LogicalType::SmallInt];
        let part = encoded_count_partition(
            &mut EncodedCountRuns { runs: vec![partition] },
            &dictionary,
            &leading,
            10,
            &Memory::unlimited(),
        )
        .expect("the encoded partition");
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for chunk in part.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row| format!("{row:?}"));
        let mut expected = vec![
            vec![Value::SmallInt(1), Value::Varchar("one".into()), Value::BigInt(2)],
            vec![Value::SmallInt(1), Value::Varchar("two".into()), Value::BigInt(1)],
            vec![Value::Null, Value::Varchar("one".into()), Value::BigInt(1)],
        ];
        expected.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(rows, expected);
    }

    #[test]
    fn a_bigint_and_stable_string_top_count_takes_the_encoded_path() {
        let plan = Plan::parse(concat!(
            "Aggregate #1 groups=[#0.0::BIGINT, #0.1::VARCHAR] ",
            "aggregates=[count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [x::BIGINT, y::VARCHAR]",
        ))
        .expect("a two-key count plan");
        let schema = Schema::numbered(
            vec![Field::new("x", LogicalType::BigInt), Field::new("y", LogicalType::Varchar)],
            0,
        );
        let rudb_plan::Node::Aggregate { groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, groups, aggregates, &Memory::unlimited())
                .expect("a count aggregate");
        let aggregate = aggregate.top_counts(10, 0);
        let users = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(7), Value::BigInt(7), Value::BigInt(8)],
        )
        .expect("user ids");
        let dictionary = Arc::new(
            Vector::from_values(
                LogicalType::Varchar,
                &[Value::Varchar("one".into()), Value::Varchar("two".into())],
            )
            .expect("search phrases"),
        );
        let phrases = Vector::stable_dictionary(vec![0, 0, 1], dictionary)
            .expect("stable search phrase codes");
        let input = Chunk::new(vec![users, phrases]).expect("two aligned columns");
        let mut local = aggregate.local();
        aggregate.sink(&input, &mut local).expect("three rows");
        aggregate.combine(local).expect("the one instance");
        assert!(aggregate.encoded_count.get().is_some(), "the compact path was selected");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");
        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{row:?}"));
        let mut expected = vec![
            vec![Value::BigInt(7), Value::Varchar("one".into()), Value::BigInt(2)],
            vec![Value::BigInt(8), Value::Varchar("two".into()), Value::BigInt(1)],
        ];
        expected.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(rows, expected);
    }

    /// A radix partition keeps no validity until it sees its first null, and fills in what it did
    /// not keep when it does. Filling in behind nothing fills in nothing, so a partition whose very
    /// first record is the null one used to keep no validity at all and read that record back as
    /// valid, holding the zero a record carries where a null was. That is a group nobody asked for
    /// and a count missing from the group that should have had it.
    #[test]
    fn a_null_in_the_first_record_of_a_partition_is_kept() {
        let mut encoded = EncodedCountPartition::default();
        let row = EncodedCountRecord { first: 0, second: 0, hash: 7, third: 0 };
        let some = EncodedCountRecord::SECOND | EncodedCountRecord::THIRD;
        encoded.push(row, some);
        encoded.push(row, EncodedCountRecord::ALL);
        assert_eq!(encoded.validity, vec![some, EncodedCountRecord::ALL]);
        let mut fixed = FixedPartition::default();
        let row = FixedRecord { first: 0, second: 0, sum: 0, mean: 0 };
        let some = FixedRecord::SECOND | FixedRecord::SUM | FixedRecord::MEAN;
        fixed.push(row, some);
        fixed.push(row, FixedRecord::ALL);
        assert_eq!(fixed.validity, vec![some, FixedRecord::ALL]);
    }

    /// Counts two `VARCHAR` keys over however many chunks are handed over, sorted.
    ///
    /// The shape of TPC-H q1's group by, which is the one the direct map over codes was written
    /// for. Every test below runs the same rows twice, once as dictionaries and once flat, and
    /// asserts the two answers are the same, because the map is only worth having if it cannot be
    /// told apart from the probe it replaces.
    fn two_key_counts(chunks: Vec<Chunk>) -> Vec<Vec<Value>> {
        let plan = Plan::parse(concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR, #0.1::VARCHAR] ",
            "aggregates=[count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::VARCHAR]",
        ))
        .expect("a two-key count plan");
        let schema = Schema::numbered(
            vec![Field::new("a", LogicalType::Varchar), Field::new("b", LogicalType::Varchar)],
            0,
        );
        let rudb_plan::Node::Aggregate { groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, groups, aggregates, &Memory::unlimited())
                .expect("a count aggregate");
        let mut local = aggregate.local();
        for chunk in &chunks {
            aggregate.sink(chunk, &mut local).expect("a chunk of rows");
        }
        aggregate.combine(local).expect("the one instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");
        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }

    /// One column of strings, flat.
    fn letters(values: &[Option<&str>]) -> Vector {
        let values: Vec<Value> = values
            .iter()
            .map(|value| match value {
                Some(text) => Value::Varchar((*text).into()),
                None => Value::Null,
            })
            .collect();
        Vector::from_values(LogicalType::Varchar, &values).expect("a column of strings")
    }

    /// The same column as codes into a dictionary of its own, which is the form a Parquet scan
    /// hands a low cardinality string column over in.
    fn letters_coded(codes: Vec<u32>, values: &[Option<&str>]) -> Vector {
        Vector::dictionary(codes, letters(values)).expect("a dictionary of those strings")
    }

    /// The rows of one chunk spelled out both ways, so a test can run the same data through the
    /// map and through the probe.
    fn both_ways(
        codes: &[(u32, u32)],
        first: &[Option<&str>],
        second: &[Option<&str>],
    ) -> (Chunk, Chunk) {
        let coded = Chunk::new(vec![
            letters_coded(codes.iter().map(|&(left, _)| left).collect(), first),
            letters_coded(codes.iter().map(|&(_, right)| right).collect(), second),
        ])
        .expect("two aligned columns");
        let flat = Chunk::new(vec![
            letters(&codes.iter().map(|&(left, _)| first[left as usize]).collect::<Vec<_>>()),
            letters(&codes.iter().map(|&(_, right)| second[right as usize]).collect::<Vec<_>>()),
        ])
        .expect("two aligned columns");
        (coded, flat)
    }

    /// q1's own key, over two chunks that share their dictionaries, which is what the chunks of one
    /// row group look like. The second chunk is where the map earns its keep: every combination in
    /// it was seen in the first, so not one of its rows is hashed or compared.
    #[test]
    fn a_group_by_over_two_dictionaries_counts_what_the_flat_columns_count() {
        let flags = [Some("A"), Some("N"), Some("R")];
        let status = [Some("F"), Some("O")];
        let (first_coded, first_flat) =
            both_ways(&[(0, 0), (1, 1), (2, 0), (0, 0)], &flags, &status);
        let (second_coded, second_flat) = both_ways(&[(1, 1), (1, 0), (2, 0)], &flags, &status);
        let counted = two_key_counts(vec![first_coded, second_coded]);
        assert_eq!(counted, two_key_counts(vec![first_flat, second_flat]));
        assert_eq!(counted.len(), 4, "A/F, N/O, R/F, N/F and nothing else");
    }

    /// A row group ends and the next one brings its own dictionary, in which the same string has a
    /// different code. A map kept across that boundary would answer the second row group with the
    /// first one's groups, which is the answer coming back with the wrong strings in it.
    #[test]
    fn a_second_dictionary_does_not_inherit_the_first_one_s_map() {
        let first = [Some("A"), Some("N")];
        let second = [Some("N"), Some("A")];
        let both = [Some("F"), Some("O")];
        let (first_coded, first_flat) = both_ways(&[(0, 0), (1, 1)], &first, &both);
        let (second_coded, second_flat) = both_ways(&[(0, 0), (1, 1)], &second, &both);
        let coded = two_key_counts(vec![first_coded, second_coded]);
        assert_eq!(coded, two_key_counts(vec![first_flat, second_flat]));
        assert_eq!(coded.len(), 4, "A/F, N/O, N/F and A/O");
    }

    /// Nulls, which reach the map two ways: a row whose own bit says it is one, and a row whose
    /// code points at a value that is one. Both are the same group and the flat columns say so.
    #[test]
    fn null_keys_behind_a_dictionary_group_the_way_flat_nulls_do() {
        let flags = [Some("A"), None];
        let status = [Some("F"), None];
        let (coded, flat) = both_ways(&[(0, 0), (1, 0), (1, 1), (0, 1)], &flags, &status);
        let counted = two_key_counts(vec![coded]);
        assert_eq!(counted, two_key_counts(vec![flat]));
        assert_eq!(counted.len(), 4);
    }

    /// Runs one chunk through the encoded count scatter and gives back the answer, sorted.
    fn encoded_count_answer(users: Vector, phrases: Vector) -> Vec<Vec<Value>> {
        let plan = Plan::parse(concat!(
            "Aggregate #1 groups=[#0.0::BIGINT, #0.1::VARCHAR] ",
            "aggregates=[count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [x::BIGINT, y::VARCHAR]",
        ))
        .expect("a two-key count plan");
        let schema = Schema::numbered(
            vec![Field::new("x", LogicalType::BigInt), Field::new("y", LogicalType::Varchar)],
            0,
        );
        let rudb_plan::Node::Aggregate { groups, aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate")
        };
        let (aggregate, out) =
            Aggregate::new(&plan, &schema, 1, groups, aggregates, &Memory::unlimited())
                .expect("a count aggregate");
        let aggregate = aggregate.top_counts(10, 0);
        let input = Chunk::new(vec![users, phrases]).expect("two aligned columns");
        let mut local = aggregate.local();
        aggregate.sink(&input, &mut local).expect("the chunk");
        aggregate.combine(local).expect("the one instance");
        assert!(aggregate.encoded_count.get().is_some(), "the compact path was selected");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");
        let mut rows = answer(&out);
        rows.sort_by_key(|row| format!("{row:?}"));
        rows
    }

    /// The scatter has two loops that have to agree about what a group is. The first reads the words
    /// the chunk holds and the second asks the vector a row at a time, and which one a chunk gets is
    /// decided by the form its columns turned up in, so the same rows in two forms are the same
    /// query down two paths and the answer cannot depend on which.
    #[test]
    fn the_run_reader_and_the_row_at_a_time_scatter_count_the_same_groups() {
        let phrases = || {
            let dictionary = Arc::new(
                Vector::from_values(
                    LogicalType::Varchar,
                    &[Value::Varchar("one".into()), Value::Varchar("two".into())],
                )
                .expect("search phrases"),
            );
            Vector::stable_dictionary(vec![0, 0, 1], dictionary).expect("stable codes")
        };
        let values = [Value::BigInt(7), Value::BigInt(7), Value::BigInt(8)];
        let flat = Vector::from_values(LogicalType::BigInt, &values).expect("user ids");
        let coded = Vector::stable_dictionary(
            vec![0, 0, 1],
            Arc::new(
                Vector::from_values(LogicalType::BigInt, &[Value::BigInt(7), Value::BigInt(8)])
                    .expect("distinct user ids"),
            ),
        )
        .expect("coded user ids");
        assert!(Signed::of(&flat, 3).is_some(), "a flat key is read as a run of words");
        assert!(Signed::of(&coded, 3).is_none(), "a coded key goes down the row at a time loop");
        assert_eq!(encoded_count_answer(flat, phrases()), encoded_count_answer(coded, phrases()));
    }

    /// A null in a leading key is what sends a chunk down the row at a time loop, since the run
    /// reader has nowhere to put one, and the group it opens is still its own group.
    #[test]
    fn a_null_leading_key_still_gets_a_group_of_its_own() {
        let dictionary = Arc::new(
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("one".into())])
                .expect("search phrases"),
        );
        let phrases = Vector::stable_dictionary(vec![0, 0, 0], dictionary).expect("stable codes");
        let users =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(7), Value::Null, Value::Null])
                .expect("user ids");
        assert!(Signed::of(&users, 3).is_none(), "a key with a null is not read as a run of words");
        assert_eq!(
            encoded_count_answer(users, phrases),
            vec![
                vec![Value::BigInt(7), Value::Varchar("one".into()), Value::BigInt(1)],
                vec![Value::Null, Value::Varchar("one".into()), Value::BigInt(2)],
            ]
        );
    }

    #[test]
    fn fixed_radix_partition_aggregates_collisions_and_nulls_exactly() {
        let mut partition = FixedPartition::default();
        let row = |first, second, sum, mean| FixedRecord { first, second, sum, mean };
        partition.push(row(1, 2, 3, 4), FixedRecord::ALL);
        partition
            .push(row(1, 2, 5, 0), FixedRecord::FIRST | FixedRecord::SECOND | FixedRecord::SUM);
        partition.push(row(0, 2, 0, 6), FixedRecord::SECOND | FixedRecord::MEAN);
        partition.push(row(0, 2, 7, 8), FixedRecord::SECOND | FixedRecord::SUM | FixedRecord::MEAN);
        let calls = [
            Call {
                name: "count_star".into(),
                args: Vec::new(),
                distinct: false,
                filter: None,
                returns: LogicalType::BigInt,
                affine: None,
            },
            Call {
                name: "sum".into(),
                args: Vec::new(),
                distinct: false,
                filter: None,
                returns: LogicalType::HugeInt,
                affine: None,
            },
            Call {
                name: "avg".into(),
                args: Vec::new(),
                distinct: false,
                filter: None,
                returns: LogicalType::Double,
                affine: None,
            },
        ];

        // q30's key shape. The record held the first key as eight bytes and the emit has to hand it
        // back two bytes wide, or the answer has the wrong column type in it.
        let keys = [LogicalType::SmallInt, LogicalType::Integer];
        let part = fixed_partition(
            &mut FixedRuns { runs: vec![partition] },
            &keys,
            10,
            &calls,
            &Memory::unlimited(),
        )
        .expect("the fixed partition");
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for chunk in part.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [
                vec![
                    Value::Null,
                    Value::Integer(2),
                    Value::BigInt(2),
                    Value::HugeInt(7),
                    Value::Double(7.0),
                ],
                vec![
                    Value::SmallInt(1),
                    Value::Integer(2),
                    Value::BigInt(2),
                    Value::HugeInt(8),
                    Value::Double(4.0),
                ],
            ]
        );
        assert_eq!(size_of::<FixedRecord>(), 16);
    }
}
