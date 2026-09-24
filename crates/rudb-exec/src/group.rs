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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use rudb_common::{
    Error, Field, LogicalType, Memory, PhysicalType, Reservation, Result, Session, Stage, Value,
    stage,
};
use rudb_kernels::{
    Accumulator, NOWHERE, finish_run, group_tally, is_true, settle_extremes, update_general,
    update_runs, update_shared_runs, update_tallied, whole_answers,
};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Data, Form, Selection, VECTOR_SIZE, Validity, Vector};

use crate::blocks::{self, Blocks};
use crate::buffer::Buffered;
use crate::group_count;
use crate::group_distinct;
use crate::group_mixed;
use crate::group_ranged;
use crate::key::{BigIntSet, Key, RowSet, mix, spread};
use crate::pairs::{self, together};
use crate::places::Places;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::signed::SignedBlock;
use crate::spill::{Reader, Spill};
use crate::table::{Origin, Probe, Table, Walk, held_at, slot_at};

/// One aggregate call, taken apart once when the operator is built.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<ExprRef>,
    distinct: bool,
    filter: Option<ExprRef>,
    returns: LogicalType,
    affine: Option<(usize, i64)>,
    reads_total: Option<usize>,
    repeats: Option<usize>,
}

impl Call {
    /// Whether this call folds rows of its own, rather than finishing out of another call's state.
    ///
    /// A call that does not fold has no argument evaluated for it, no accumulator updated for it and
    /// no run at a time finish taken for it, so this is asked at each of those places rather than
    /// each of them asking about the three ways a call can be derived.
    fn folds(&self) -> bool {
        self.affine.is_none() && self.reads_total.is_none() && self.repeats.is_none()
    }

    /// The call whose state this one finishes out of, which is itself when it folds its own.
    ///
    /// A repeated call is the same accumulator as the one it repeats, so it finishes the same way
    /// off the same state and the only difference is which state it reads. The other two derived
    /// shapes finish differently from their source and are asked about where they are finished.
    fn state_of(&self, at: usize) -> usize {
        self.repeats.unwrap_or(at)
    }

    /// Whether this call's answer is a state finished the ordinary way, off whichever state it reads.
    ///
    /// True of a call that folds its own and of one that repeats an earlier one, and false of the two
    /// that finish differently from the state they read: an affine call adds an offset per row to the
    /// total it finds, and a sum read off a mean pulls the exact total out of it. So this is what the
    /// run at a time finish asks, where every other place asks [`Call::folds`].
    fn finishes_plainly(&self) -> bool {
        self.affine.is_none() && self.reads_total.is_none()
    }
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

/// Marks `sum(X)` calls that can read their total out of an `avg(X)` in the same aggregate.
///
/// A sum and a mean of one column add the same numbers up, and the mean keeps the count it is going
/// to divide by besides, so a query asking for both only has to fold the column once. q01 asks for
/// the sum and the mean of `l_quantity` and of `l_extendedprice`, and folding each of those columns
/// twice was 266.6M of its 2786.2M instructions at SF1. The same pair of queries costs DuckDB the
/// same either way, so it already does this.
///
/// The sum is what gets marked rather than the mean, because a mean's state keeps the total and the
/// count and a sum's keeps only the total, so this is the direction that needs nothing added to a
/// state. A `DISTINCT` or a `FILTER` on either call makes the two folds run over different rows. A
/// mean that has gone inexact has no exact total left to read, and the finish is where that is found
/// out, because whether it happens depends on the rows rather than on the plan.
///
/// Only over a column of whole numbers, which is an integer or a decimal. A mean of those adds the
/// unscaled integers up in an `i128` and divides once, and that total is the number a sum of the same
/// column reaches. A mean over floating point is not: it adds in floating point in the order the rows
/// arrive, the way a sum of it does, and a sum read out of anything else would round differently on
/// some columns and agree on others. So the two are only ever shared where they are the same
/// addition.
fn mark_sums_from_means(plan: &Plan, calls: &mut [Call]) {
    let whole = |call: &Call| {
        let [argument] = call.args.as_slice() else { return false };
        let of = plan.expr_type(*argument);
        of.is_integer() || matches!(of, LogicalType::Decimal { .. })
    };
    for at in 0..calls.len() {
        if calls[at].name != "sum"
            || calls[at].distinct
            || calls[at].filter.is_some()
            || calls[at].affine.is_some()
            || !whole(&calls[at])
        {
            continue;
        }
        // A sum another call reads as its own base still has to fold, because what that call adds an
        // offset to is this one's accumulator rather than its answer.
        if calls.iter().any(|call| call.affine.is_some_and(|(source, _)| source == at)) {
            continue;
        }
        calls[at].reads_total = (0..calls.len()).find(|&source| {
            calls[source].name == "avg"
                && !calls[source].distinct
                && calls[source].filter.is_none()
                && calls[source].folds()
                && calls[source].args == calls[at].args
        });
    }
}

/// Marks calls that are the same call as an earlier one in the same aggregate, so it folds once.
///
/// TPC-H q01 asks for `count(*)` and for the average of two columns it also sums. The common
/// aggregate pass reads each of those averages off its sum, which leaves a count of the summed
/// column behind for the division, and both columns are `NOT NULL`, so the null free pass turns both
/// of those counts into `count(*)` as well. The plan reaches here asking for the same row count
/// three times, and three counters were kept and three answers written where one counter and three
/// reads of it do.
///
/// Every duplicate that gets here comes out of a rewrite rather than out of a query. A query that
/// writes the same call twice is bound to one slot in the aggregate and two references to it from the
/// projection above, so the two never reach this. What the rewrites do is add a call after that,
/// which is the count each shared average needs, and turn a call into another one, which is
/// `count(x)` over a column with no nulls becoming `count(*)`. So the plainest case is a query that
/// averages a column and counts it: `SUM(x), AVG(x), COUNT(x)` arrives here as `sum(x), count(x),
/// count(x)`.
///
/// Nothing above the operator sees any of it. The output still has one column per call in the order
/// the plan asked for them, and the repeated column is the same finish taken off the earlier call's
/// state.
///
/// The same call means the same name, the same argument expressions, the same `FILTER` and the same
/// declared return type, so the two accumulators would be built the same way and fed the same rows.
/// Arguments are compared by expression reference the way [`mark_sums_from_means`] compares them,
/// which is the conservative half of the question: two references to one expression are the same
/// expression, and two expressions that happen to be spelled the same are left alone.
///
/// A `DISTINCT` call is refused. Its state is a set of values rather than an accumulator and the
/// operator has three separate shapes for reading distinct counts a column at a time, so sharing
/// one would have to be true of all of them rather than of the finish.
///
/// This runs after the other two markers and not before them. `sum(x), sum(x), avg(x)` has both sums
/// reading the mean's total, which is one fold for all three, and a pass that pointed the second sum
/// at the first one first would have pointed it at a call that then stopped folding.
fn mark_repeated_calls(calls: &mut [Call]) {
    for at in 0..calls.len() {
        if calls[at].distinct || !calls[at].folds() {
            continue;
        }
        // A call another one is derived from still has to fold, because what that call reads is this
        // one's accumulator rather than its answer.
        let source_of_another = calls.iter().any(|call| {
            call.affine.is_some_and(|(source, _)| source == at) || call.reads_total == Some(at)
        });
        if source_of_another {
            continue;
        }
        calls[at].repeats = (0..at).find(|&source| {
            let earlier = &calls[source];
            earlier.folds()
                && !earlier.distinct
                && earlier.name == calls[at].name
                && earlier.args == calls[at].args
                && earlier.filter == calls[at].filter
                && earlier.returns == calls[at].returns
        });
    }
}

/// The answer a `sum` marked by [`mark_sums_from_means`] gives, out of the mean's state.
///
/// Its own function, and kept out of line, because it builds a whole accumulator and it is reached
/// once a group for the one call in a query that was marked, while the loop that calls it finishes
/// every call of every group. Written inline in that loop the building goes into the loop's body for
/// every query, marked or not, and that loop already turns out to be sensitive to how much code is
/// around it. See #1730 for what that sensitivity cost once. Neither shape measured differently on
/// the queries here, so this is the one that keeps the cold path out of the hot one by construction
/// rather than by an optimiser's opinion.
///
/// # Errors
///
/// A mean that has gone inexact, which is a total that did not fit an `i128`. A sum of its own would
/// have raised on the row that stopped fitting rather than carrying on in floating point, so this
/// raises too rather than answering something a real sum would not have answered.
#[inline(never)]
fn sum_from_mean(held: &Accumulator, returns: &LogicalType) -> Result<Value> {
    let Some((total, seen)) = held.exact_total() else {
        return Err(Error::out_of_range("a total too large for an exact sum".to_string()));
    };
    Accumulator::sum_of(total, seen, returns)?.finish()
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
    /// The state a new group starts in, one per call, or nothing if a call has no accumulator.
    ///
    /// Reading an aggregate's name and return type to decide what its state is does not depend on
    /// the group, so doing it per group was a six way string match a million times over to reach a
    /// million equal answers. It is one clone from here instead.
    template: Option<Vec<Accumulator>>,
    inputs: Prepared,
    schema: Schema,
    /// Whether there are no group expressions, so every row goes to the one slot.
    alone: bool,
    /// How many groups an instance holds before it partitions, which is [`PARTITION_FROM`] or
    /// [`FIXED_PARTITION_FROM`] by the width of the keys.
    partition_from: usize,
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
    /// How many groups to take room for before the first row arrives, where the planner said.
    ///
    /// A capacity and never a count. The table holds whatever the keys put in it and this changes
    /// only how many times it has to grow on the way there, so a number that is wrong costs the
    /// growing it was meant to save and costs nothing else. `rudb_opt`'s `presize` pass is where it
    /// comes from and where the reasoning about which numbers are worth acting on lives.
    ///
    /// Groups for the whole aggregate, so a table built for a radix partition takes its [`Share`]
    /// of this rather than all of it.
    presize: Option<u64>,
    /// Whether a table taking room for [`Aggregate::presize`] reserves it against the budget before
    /// it asks the allocator, and starts at the ordinary size when the budget cannot hold it.
    ///
    /// `spec/stats/05-every-query.md` section 5.1 is the reason it can fall back rather than fail.
    /// Room taken for groups that may never come is the one charge in the operator that is a choice,
    /// and a choice that does not fit is made the other way: a table that starts small grows the
    /// way it always did, which costs time, where one charged on its first chunk for room it never
    /// fills turns a query that fits into one that is out of memory. [`Rule::MemoryReservation`] is
    /// the switch.
    ///
    /// [`Rule::MemoryReservation`]: rudb_common::rules::Rule::MemoryReservation
    reserve: bool,
    /// The range the one integer grouping key lies in, where the planner said it has one.
    ///
    /// A shortcut and never a rule. The table it reaches holds the same groups in the same slots
    /// whether or not it hears this, and a key value the range does not cover is looked up the way
    /// it always was. `rudb_opt`'s `dense` pass is where it comes from and where the reasoning about
    /// which ranges are worth acting on lives.
    ///
    /// Not to be confused with `dense` below, which is a different structure for a different case:
    /// that one is a grouped count over a stable dictionary, keyed by the storage code, and it
    /// replaces the hash table rather than sitting beside it.
    span: Option<(i128, u64)>,
    /// The ends of the one integer grouping key where `span` was turned down for being sparse,
    /// which the map a chunk's key values are placed in is built over from the start. See
    /// [`Aggregate::within`].
    ends: Option<(i128, u64)>,
    /// Whether the one grouping key arrives in ascending order, so a group whose key the rows have
    /// moved past is finished and can skip the table. `rudb_opt`'s `cluster` pass is where it comes
    /// from, and [`interior`] checks every chunk before believing it.
    clustered: bool,
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
    /// Keys and run weights exchanged for a single signed integer key's `COUNT(*)`.
    counted: OnceLock<group_count::Exchange>,
    /// Counts in arrays the key indexes, for a key whose range is known. See [`group_ranged`].
    ranged: OnceLock<group_ranged::Exchange>,
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
    runs: Vec<Blocks<u32>>,
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
    runs: Vec<EncodedCountRun>,
}

/// One radix partition's records for the fixed exchange, one run per instance. See
/// [`EncodedCountRuns`], which this is the same thing as for a different record.
#[derive(Debug, Default)]
struct FixedRuns {
    runs: Vec<FixedRun>,
}

/// The slots of the `bound` largest of `groups` counts, largest first and, among equal counts, in
/// slot order.
///
/// Kept as a sorted list rather than a heap because the callers want it in that order, and a slot
/// that cannot make the list is ruled out against its last entry before any search. That is most of
/// them: on ClickBench 40 a partition is thousands of groups under a bound of 1010, nearly all of
/// them seen once, and every one used to pay a binary search through the list to be told so.
pub(crate) fn largest<T: Ord>(
    groups: usize,
    bound: usize,
    count: impl Fn(usize) -> T,
) -> Vec<usize> {
    let mut best: Vec<usize> = Vec::with_capacity(bound.min(groups));
    if bound == 0 {
        return best;
    }
    for slot in 0..groups {
        let value = count(slot);
        if best.len() == bound && best.last().is_some_and(|&last| count(last) >= value) {
            continue;
        }
        let at = best.partition_point(|&kept| count(kept) >= value);
        best.insert(at, slot);
        best.truncate(bound);
    }
    best
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

/// The bit that stands for the call at this offset in a set of calls, and zero for an offset too far
/// along to have one.
///
/// The sets are the ones [`rudb_kernels::update_shared_runs`] is offered and answers with, and zero for
/// an offset past sixty four leaves that call out of the sharing and folded the way it was folded
/// before. No plan reaches it: sixty four folding aggregates in one `GROUP BY` is far past the point
/// where finding runs at all pays for itself.
fn one_call(at: usize) -> u64 {
    u32::try_from(at).ok().and_then(|at| 1_u64.checked_shl(at)).unwrap_or(0)
}

/// Cuts a chunk's slots into runs of one slot, each given as its slot and the row it ends before,
/// when they come in runs long enough for the `users` calls that will read them to pay for the pass.
///
/// One pass, a block of [`RUN_BLOCK`] slots at a time, and no branch on any row. A row starts a run
/// where its slot differs from the slot before it, which is a compare per row and nothing carried from
/// one row to the next, so the block's starts are gathered into a mask of one bit a row and the runs
/// are read off it by counting its trailing zeroes. A run that ends before row `at` has the slot row
/// `at - 1` holds, so the mask alone says both numbers a run is and the walk never tracks which slot it
/// is in.
///
/// Per row a branch is what this used to cost. The version before it or'd sixteen differences against
/// the slot of the run it was in and walked a row at a time only where that or came out non zero, which
/// reads well and does nothing on a chunk like q01's: runs of 2.81 rows change slot in every block
/// there is, so the or never once skipped a block and every row paid a compare the branch predictor
/// cannot call, a third of them taken. It was 16 percent of the fold on q01 and most of that was the
/// mispredict.
///
/// A chunk of keys in no order is given up on as soon as it has more runs than it is allowed, checked
/// once a block against the bits the mask has set rather than once a run. `false`, with `into` empty,
/// for a chunk that is not worth it.
///
/// The budget is [`RUN_ROWS`] rows a run for each call that will read the runs, because the pass is
/// paid once a chunk however many read it and each one that does saves a pass of its own. q01 reads
/// them eight times over a mean run of 2.81 rows, and a flat eight rows a run gave up on every chunk
/// of it, while q09 and q13 read them once and cutting their chunks costs more than it returns. See
/// #1633.
fn slot_runs_of(slots: &[usize], into: &mut Vec<(usize, usize)>, users: usize) -> bool {
    into.clear();
    let Some(&last) = slots.last() else {
        return false;
    };
    let most = slots.len().saturating_mul(users) / RUN_ROWS;
    // Room for every run the budget allows, since a chunk cannot hold more runs than it has rows, so
    // the push below grows nothing and the walk is the compare and the store it looks like.
    into.reserve(most.min(slots.len()) + 1);
    let mut row = 1;
    while row < slots.len() {
        let end = (row + RUN_BLOCK).min(slots.len());
        let block = &slots[row..end];
        let prior = &slots[row - 1..end - 1];
        let mut starts = 0_u64;
        for (at, (&slot, &before)) in block.iter().zip(prior).enumerate() {
            starts |= u64::from(slot != before) << at;
        }
        if into.len() + starts.count_ones() as usize > most {
            into.clear();
            return false;
        }
        while starts != 0 {
            let at = starts.trailing_zeros() as usize;
            starts &= starts - 1;
            into.push((prior[at], row + at));
        }
        row = end;
    }
    into.push((last, slots.len()));
    true
}

/// How many slots [`slot_runs_of`] reads a mask of starts over, which is one bit a row of a `u64`.
const RUN_BLOCK: usize = 64;

/// How many rows a run of one slot has to hold on average, per call that will read the runs, for
/// [`slot_runs_of`] to cut a chunk into runs, which is where folding a run at once costs less than
/// the pass that finds them.
const RUN_ROWS: usize = 8;

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
    /// How many rows each record stands for. Empty while every record is one row, which it is until
    /// the run is first compacted.
    weights: Vec<u32>,
}

/// How long a scattered run gets before it is compacted rather than grown.
///
/// A run holds a record a row, so a grouped count over ten million rows scattered 240 MB of
/// records before the fold ever saw them, and the doubling behind it held up to twice that. When a
/// run fills, the records it already holds are folded in place into one per group with a weight,
/// and the run grows only if that did not free a quarter of it. On ClickBench `GROUP BY UserID,
/// SearchPhrase` a group is four rows on average and they arrive close together, so most of a run
/// folds away. The floor keeps small runs, which fit in cache and cost nothing to hold, out of it.
const COMPACT_FROM: usize = 4_096;

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
        if !self.weights.is_empty() {
            self.weights.push(1);
        }
    }

    /// Takes one record that stands for `weight` rows, and starts keeping weights at the first one
    /// that is not a single row.
    fn push_weighted(&mut self, row: EncodedCountRecord, valid: u8, weight: u32) {
        if weight != 1 && self.weights.is_empty() {
            self.weights.resize(self.rows.len(), 1);
        }
        let weighing = !self.weights.is_empty();
        self.push(row, valid);
        if weighing {
            *self.weights.last_mut().expect("a weight was just pushed") = weight;
        }
    }

    /// How many rows the record at `at` stands for.
    #[inline]
    fn weight(&self, at: usize) -> i64 {
        self.weights.get(at).map_or(1, |&weight| i64::from(weight))
    }
}

/// One instance's scattered records for one radix partition.
///
/// Held in [`Blocks`] rather than vectors, because a run that has folded what it can keeps growing
/// until the instance is done, and every run of an instance grows in step. As a vector each growth
/// was a doubling that left the run up to half empty with the buffer it came from resident beside
/// it: on ClickBench q19, five million groups over ten million rows, one thread held 201 MB of
/// record capacity for about 118 MB of records.
///
/// The groups a compaction found and the records that arrived since are kept apart. A compaction
/// only reads the new records through once, in order, and adds what starts a new group to the end
/// of the groups, so neither list is rewritten in place, and only the groups carry a weight.
#[derive(Debug)]
struct EncodedCountRun {
    /// One record a group for the groups the last compaction found, in the order they arrived.
    groups: Blocks<EncodedCountRecord>,
    /// How many rows each group stands for, in step with `groups`.
    weights: Blocks<u32>,
    /// The validity of each group, empty while all three keys of every group are valid.
    group_validity: Vec<u8>,
    /// The records scattered since the last compaction, each of them one row.
    pending: Blocks<EncodedCountRecord>,
    /// Empty while all three keys are valid, as in [`EncodedCountPartition`].
    pending_validity: Vec<u8>,
    /// How many records are taken before the run is compacted again.
    room: usize,
}

impl Default for EncodedCountRun {
    fn default() -> Self {
        Self {
            groups: Blocks::default(),
            weights: Blocks::default(),
            group_validity: Vec::new(),
            pending: Blocks::default(),
            pending_validity: Vec::new(),
            room: COMPACT_FROM,
        }
    }
}

impl EncodedCountRun {
    /// Takes one record, and starts keeping validity if this is the first null this has seen, for the
    /// reason [`EncodedCountPartition::push`] gives.
    fn push(&mut self, row: EncodedCountRecord, valid: u8) {
        let keeping = !self.pending_validity.is_empty() || valid != EncodedCountRecord::ALL;
        if keeping {
            self.pending_validity.resize(self.pending.len(), EncodedCountRecord::ALL);
        }
        self.pending.push(row);
        if keeping {
            self.pending_validity.push(valid);
        }
    }

    /// Takes one scattered record, compacting the run first when it has grown to its limit.
    #[inline]
    fn scatter(&mut self, row: EncodedCountRecord, valid: u8) {
        if self.pending.len() >= self.room {
            self.compact();
        }
        self.push(row, valid);
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.pending.is_empty()
    }

    fn len(&self) -> usize {
        self.groups.len() + self.pending.len()
    }

    /// Folds the records that arrived since the last compaction into the groups, adding up weights.
    ///
    /// A weight that would pass `u32::MAX` is left as a group of its own, which the fold at the end
    /// adds up like any other. The run is let grow to twice its length before the next compaction
    /// if this did not free a quarter of it, which is when a vector used to double.
    #[cold]
    fn compact(&mut self) {
        let len = self.len();
        // A bucket holds the group's block and its place in the block in [`SLOT_BITS`], packed as
        // [`blocks::WITHIN_BITS`] says so a match is found with a shift rather than by working out
        // which block a position falls in, and a run too long for that is left to the fold at the
        // end, which is where it would have gone had it never been compacted. The last block that
        // fits is left out too, because its last place is [`EMPTY_SLOT`].
        if blocks::locate(len).0 >= (SLOT_MASK >> blocks::WITHIN_BITS) as usize {
            self.room = len;
            return;
        }
        let capacity = len.saturating_mul(2).next_power_of_two();
        let mask = capacity - 1;
        let mut buckets = vec![EMPTY_SLOT; capacity];
        // A tag of eight hash bits beside the position, for the reason [`slot_tag`] gives, so a
        // bucket of another group is passed over without finding its record in the blocks. Not the
        // bits `slot_tag` takes, because the top six of those pick the radix partition and are the
        // same for every record in a run, but the eight below them, which the slot of a table under
        // a quarter million buckets does not use either.
        let tag = |hash: u32| ((hash >> 18) & 0xff) << SLOT_BITS;
        let place = |block: usize, within: usize| (block << blocks::WITHIN_BITS | within) as u32;
        // The groups are one record a group already, so they go into the table without being
        // compared with anything.
        for (block, values) in self.groups.slices().enumerate() {
            for (within, row) in values.iter().enumerate() {
                let mut slot = row.hash as usize & mask;
                while buckets[slot] != EMPTY_SLOT {
                    slot = (slot + 1) & mask;
                }
                buckets[slot] = tag(row.hash) | place(block, within);
            }
        }
        let pending_valid = self.pending_validity.is_empty();
        // Kept from the first null on, and filled in behind it, which with no groups yet is
        // nothing, so whether to keep it is not read off its length.
        let all_valid = pending_valid && self.group_validity.is_empty();
        if !all_valid {
            self.group_validity.resize(self.groups.len(), EncodedCountRecord::ALL);
        }
        let pending_validity = std::mem::take(&mut self.pending_validity);
        let (mut block, mut within) = blocks::locate(self.groups.len());
        let mut at = 0;
        // Each block of new records is let go once it has been read, so the run holds no more
        // while it compacts than it did before.
        for values in std::mem::take(&mut self.pending).into_blocks() {
            for &row in &values {
                let valid =
                    if pending_valid { EncodedCountRecord::ALL } else { pending_validity[at] };
                at += 1;
                let mut slot = row.hash as usize & mask;
                let tagged = tag(row.hash);
                loop {
                    let bucket = buckets[slot];
                    if bucket == EMPTY_SLOT {
                        buckets[slot] = tagged | place(block, within);
                        self.groups.push(row);
                        self.weights.push(1);
                        if !all_valid {
                            self.group_validity.push(valid);
                        }
                        within += 1;
                        if within == blocks::size(block) {
                            block += 1;
                            within = 0;
                        }
                        break;
                    }
                    if bucket & !SLOT_MASK == tagged {
                        let held = ((bucket & SLOT_MASK) >> blocks::WITHIN_BITS) as usize;
                        let offset =
                            (bucket & SLOT_MASK) as usize & ((1 << blocks::WITHIN_BITS) - 1);
                        let other = self.groups.slot(held, offset);
                        if other.hash == row.hash
                            && other.first == row.first
                            && other.second == row.second
                            && other.third == row.third
                            && (all_valid
                                || self.group_validity[blocks::start(held) + offset] == valid)
                        {
                            let sum = self.weights.slot_mut(held, offset);
                            if let Some(total) = sum.checked_add(1) {
                                *sum = total;
                                break;
                            }
                        }
                    }
                    slot = (slot + 1) & mask;
                }
            }
        }
        let kept = self.groups.len();
        let limit = if kept * 4 > len * 3 { len * 2 } else { len };
        self.room = limit - kept;
    }

    fn footprint(&self) -> usize {
        self.groups.footprint()
            + self.weights.footprint()
            + self.group_validity.capacity() * size_of::<u8>()
            + self.pending.footprint()
            + self.pending_validity.capacity() * size_of::<u8>()
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
}

/// What one instance scatters into one fixed radix partition, read back once and in order by the
/// fold, so its records are kept in [`Blocks`] rather than grown and copied as one vector.
#[derive(Debug, Default)]
struct FixedRun {
    rows: Blocks<FixedRecord>,
    /// Empty while every field is valid, as in [`FixedPartition`].
    validity: Vec<u8>,
}

impl FixedRun {
    /// Takes one record, the way [`FixedPartition::push`] does.
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
        self.rows.footprint() + self.validity.capacity() * size_of::<u8>()
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

/// How many codes a dense partition has to have per row that arrived before its rows are sorted
/// rather than counted into an array a code wide.
const SPARSE_DENSE: usize = 16;

/// Which of an aggregate's tables is being built, which is what says how much room it wants.
///
/// A presize is a number of groups for the whole aggregate, and one aggregate builds tables of four
/// different kinds: one an instance fills before it partitions, one per radix partition per instance
/// while the cache holds them, one per radix partition shared by every instance, and, where the
/// aggregate cannot partition at all, one that holds everything. Only two of those ever hold the
/// groups the number counts. Nothing here changes an answer: a table takes whatever the keys put in
/// it either way and this only says how much room to take before the first row arrives.
///
/// Reading the number the same way for all four was worth minus three percent on TPC-H. On the
/// `GROUP BY l_orderkey` inside q18, which is six million rows into a million and a half groups and
/// the exact case the presize pass was written for, it was 3.706 G of instructions against 5.208.
/// The room asked for was half a gigabyte: eight instances each taking room for a million and a half
/// groups in a table they give up at [`PARTITION_FROM`], and then each taking room for a sixty
/// fourth of them sixty four more times over in tables the cache cannot hold and which are handed
/// over almost at once. None of it was ever written into, and clearing the pages for it is what the
/// query spent the instructions on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Share {
    /// Every group the aggregate will produce, which is the one table it answers out of when it
    /// cannot partition.
    Whole,
    /// The table an instance fills before it partitions, which it gives up at [`PARTITION_FROM`]
    /// groups, so that is all the room there is any point taking for it.
    Passing,
    /// One of the [`RADIX_PARTITIONS`] tables the whole aggregate shares and answers out of, holding
    /// the share of the groups the hash puts in it.
    Partition,
    /// One of an instance's own per partition tables, held only while the cache holds every
    /// instance's set of them and handed over otherwise, so it takes the room every table used to
    /// start with and no more.
    Local,
}

impl Share {
    /// The groups to take room for, out of `groups` for the whole aggregate, or `None` for a table
    /// that is passing them on rather than holding them.
    fn of(self, groups: u64) -> Option<u64> {
        match self {
            Self::Whole => Some(groups),
            Self::Passing => Some(groups.min(PARTITION_FROM as u64)),
            // At least one, because a partition that ends up with a group still wants a table and
            // rounding a small aggregate down to nothing would give it the smallest one twice.
            Self::Partition => Some((groups / RADIX_PARTITIONS as u64).max(1)),
            Self::Local => None,
        }
    }

    /// Whether this is the table an instance keeps before it partitions, which is the one whose key
    /// covers the aggregate's whole range and so the only one a direct index over that range fits.
    fn before_the_split(self) -> bool {
        matches!(self, Self::Whole | Self::Passing)
    }
}

/// Whether room taken ahead of the groups can come out of the budget, which is while it leaves the
/// query using no more than half of it.
///
/// Room reserved because it fits can still be room the rest of the query needed. An aggregate that
/// partitions takes a table for each of its sixty four partitions, and when the ceiling is well over
/// the groups that arrive, each one reserving what fits leaves the budget full of empty buckets and
/// the next small allocation anywhere in the query out of memory. The half is the part of the budget
/// a guess may spend. What the rows really need is charged as it arrives and may take the rest, and
/// a table declined here starts at the ordinary size and grows, which is the failure section 5.1
/// says to prefer.
fn spare(memory: &Memory, room: u64) -> bool {
    memory.limit().is_none_or(|limit| memory.used().saturating_add(room) <= limit / 2)
}

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

/// [`PARTITION_FROM`] for an aggregate whose keys are all fixed width.
///
/// The four thousand was measured when every aggregate partitioned at the same count, and the
/// aggregates it was measured on mostly group by a string. A string group costs its payload in every
/// table it is copied into, so it pays to stop keeping it per instance early. A group of integers is
/// a few dozen bytes, and splitting it into sixty four tables is then most of what the aggregate
/// does: ClickBench 40 folds 75 thousand rows into 7 thousand groups, and each instance crossed four
/// thousand, handed every group into its partitions a row at a time and merged them again at the
/// close. At sixteen thousand it spends a fifth to a third less CPU, and ClickBench 32 and 35, which
/// also group only by integers, come out the same within the noise. The same number for string keys
/// made ClickBench 39 12 percent slower, which is why they keep the lower one.
const FIXED_PARTITION_FROM: usize = 16_384;

/// How many rows a partitioned instance puts aside before it splits them, which is a few hundred
/// rows to each of the [`RADIX_PARTITIONS`]. See [`Aggregate::drain`].
const GATHER_ROWS: usize = 16_384;

/// How many places a map read by value may clear for each row the table has folded.
///
/// Clearing a place is a store, and a row the map answers is a hash and a probe it did not do, so a
/// map that clears no more than two places for every row folded costs less than the hashing it can
/// save, and a key whose window settles, the way `CounterID` does, stops clearing at all.
const WINDOW_RATE: usize = 2;

/// The places a map read by value may clear before a row has been folded, which is enough for the
/// smallest window [`coded_within`](crate::table::coded_within) makes and a few times over.
const WINDOW_SLACK: usize = 4_096;

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
                reads_total: None,
                repeats: None,
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
        // Only where the accumulators own the calls. The four shapes above read their arguments by
        // the position of the call in the list and finish without touching an accumulator at all, so
        // a call marked as reading another's total would have its argument dropped from `inputs` and
        // then be asked for it anyway.
        if !compact_numeric && !distinct_count && !mixed_numeric_distinct && !radix_distinct_count {
            mark_sums_from_means(plan, &mut calls);
            mark_repeated_calls(&mut calls);
        }
        let mut inputs = keys.clone();
        for call in &calls {
            if call.folds() {
                inputs.extend_from_slice(&call.args);
            }
            inputs.extend(call.filter);
        }
        let inputs = Prepared::shared(plan, &inputs, &input_schema)?;
        let by_vector: Vec<bool> =
            calls.iter().map(|call| alone && !call.distinct && call.filter.is_none()).collect();
        // Every group of one operator starts from the same accumulator per call, so the one a group
        // starts with is worked out here rather than once per group. A call this cannot build is one
        // `fresh` raises on, and it still raises there rather than here, so a plan that never opens a
        // group answers the way it always did.
        let template: Option<Vec<Accumulator>> =
            calls.iter().map(|call| Accumulator::new(&call.name, &call.returns).ok()).collect();
        let out = Buffered::new();
        let partition_from = if keys.iter().all(|&key| fixed_width(plan.expr_type(key))) {
            FIXED_PARTITION_FROM
        } else {
            PARTITION_FROM
        };
        let aggregate = Self {
            plan,
            keys,
            constants,
            alone,
            partition_from,
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
            presize: None,
            reserve: false,
            span: None,
            ends: None,
            clustered: false,
            agreed: Mutex::new(None),
            settled: AtomicBool::new(false),
            by_vector,
            groups,
            calls,
            template,
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
            counted: OnceLock::new(),
            ranged: OnceLock::new(),
            out: out.clone(),
        };
        Ok((aggregate, out))
    }

    /// Stops opening groups once an unordered limit above this aggregate cannot observe another.
    pub(crate) fn limit_groups(mut self, max_groups: usize) -> Self {
        self.max_groups = Some(max_groups);
        self
    }

    /// Takes room for that many groups in each instance's table before any row arrives.
    ///
    /// Half of `spec/stats/05-every-query.md` section 5.4's first rule, the other half being the
    /// pass that works out the number. Per instance, because the tables are per instance: a
    /// partitioned aggregate on sixteen threads takes sixteen of these, which is the reason the pass
    /// has a ceiling at all.
    pub(crate) fn presize(mut self, groups: u64) -> Self {
        self.presize = Some(groups);
        self
    }

    /// Reserves the room [`Aggregate::presize`] takes before taking it, per [`Aggregate::reserve`].
    pub(crate) fn reserved(mut self) -> Self {
        self.reserve = true;
        self
    }

    /// The range the one integer grouping key lies in, from `rudb_opt`'s `dense` pass.
    pub(crate) fn over_range(mut self, low: i128, values: u64) -> Self {
        self.span = Some((low, values));
        self
    }

    /// The ends the one integer grouping key lies in, from `rudb_opt`'s `dense` pass, where the
    /// range was too sparse to be `over_range`.
    ///
    /// Left to itself the map of key values starts on the first chunk's window and grows as the
    /// values climb out of it. A sorted `CounterID` built seven maps a query that way, copying the
    /// old one at each step, and the last was a new map as wide as the range, because a window
    /// whose bottom was not the column's could not take the top in. Filled and copied, those maps
    /// were a tenth of ClickBench 28. Seeded with the ends, it is one map filled once.
    pub(crate) fn within(mut self, low: i128, values: u64) -> Self {
        self.ends = Some((low, values));
        self
    }

    /// Closes a group as soon as its key is behind the rows, from `rudb_opt`'s `cluster` pass.
    pub(crate) fn clustered(mut self) -> Self {
        self.clustered = true;
        self
    }

    /// Whether this aggregate closes groups, which is the pass having said so and nothing else
    /// here needing to see every group.
    ///
    /// The exchanges and the dense count each keep their own state and finish from it alone, so a
    /// closed group would never reach their answer. A `DISTINCT` call keeps a set per group and a
    /// pushed down limit agrees on groups between instances, and neither is worth teaching about a
    /// group that skipped the table.
    fn closes(&self) -> bool {
        self.clustered
            && !self.alone
            && !self.sets
            && self.keys.len() == 1
            && self.max_groups.is_none()
            && !self.count_only
            && !self.radix_distinct_count
            && !self.fixed_top_count()
            && !self.encoded_top_count()
            && !self.grouped_distinct_top_count()
            && !self.mixed_top_count()
            && !self.counted_top_count()
    }

    /// Whether the groups a sorted chunk closes can be answered straight from their runs, with no
    /// table and no accumulators. See [`Aggregate::close_runs`].
    ///
    /// Every call has to be a count or a total whose answer is the sum of the raw integers, which is
    /// a total over an integer column or over a decimal at the scale the total is declared at. A
    /// `FILTER` clause, a call finished from another call's state and the selections made from the
    /// counts afterwards all live in the accumulators, so any of them leaves the chunk to the table.
    fn closes_by_run(&self) -> bool {
        self.closes()
            && !self.compact_numeric
            && self.having_count.is_none()
            && self.top_counts.is_none()
            && self.calls.iter().enumerate().all(|(at, call)| {
                !call.distinct
                    && call.filter.is_none()
                    && call.folds()
                    && !self.by_vector[at]
                    && match (call.name.as_str(), call.args.as_slice()) {
                        ("count_star", []) | ("count", [_]) => true,
                        ("sum", [argument]) => {
                            whole_total(self.plan.expr_type(*argument), &call.returns)
                        }
                        _ => false,
                    }
            })
    }

    /// Rows into this instance's own table or the partitions, which is every row that is not in a
    /// closed group.
    fn open(
        &self,
        rows: &Rows,
        single: &mut Option<Building>,
        installed: &mut bool,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
        folded: &mut u64,
    ) -> Result<()> {
        *folded += rows.rows as u64;
        let Some(table) = single else {
            spreading.gathered += rows.rows;
            spreading.pending.push(rows.settled()?.into_owned());
            if spreading.gathered < GATHER_ROWS {
                return Ok(());
            }
            return self.drain(*folded, spreading, own);
        };
        if let Some(error) = table.failure.take() {
            return Err(error);
        }
        if let Some(limit) = self.max_groups {
            if !self.alone {
                self.agree(rows, limit, table, installed)?;
            }
        }
        let timing = stage::Timing::start(Stage::Fold);
        let done = self.fold(rows, table, None, None);
        timing.stop(0);
        done?;
        if !self.ought_to_partition(table) {
            return Ok(());
        }
        let handing = single.take().expect("the table was there a moment ago");
        self.begin_partitioning(spreading, own)?;
        self.hand(handing, spreading, own)
    }

    /// The table closed groups go into, which is never probed, so it takes no room in advance.
    fn shut(&self) -> Building {
        let mut local = self.starting(Share::Local);
        let types: Vec<_> = self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect();
        local.table = Table::new(&types);
        local
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
        } else if self.calls.get(call).is_some_and(|held| {
            held.name == "count_star"
                && held.args.is_empty()
                && !held.distinct
                && held.filter.is_none()
        }) {
            // The state kept rather than the call named, because a call that repeats an earlier one
            // has an accumulator nobody folded into and the running count is read straight out of a
            // state here. See [`mark_repeated_calls`].
            self.top_counts = Some((bound, self.calls[call].state_of(call)));
        }
        self
    }

    /// Drops groups below an inclusive COUNT(*) bound before result vectors are materialized.
    #[must_use]
    pub(crate) fn having_count(mut self, call: usize, minimum: i64) -> Self {
        if self.calls.get(call).is_some_and(|held| {
            held.name == "count_star"
                && held.args.is_empty()
                && !held.distinct
                && held.filter.is_none()
        }) {
            // The state kept rather than the call named, for the reason [`Aggregate::top_counts`]
            // gives above.
            self.having_count = Some((self.calls[call].state_of(call), minimum));
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
        let mut evaluated = Vec::new();
        let Some(kept) = chunk.kept() else {
            self.inputs.evaluate(chunk, scratch, &mut evaluated)?;
            return self.rows_of(evaluated, chunk.len(), None);
        };
        // A marked chunk has every row still in its columns, and the arguments are read over all
        // of them. One that raises there may be raising at a row the filter dropped, so the chunk
        // is cut to the kept rows and read again, which raises exactly where a cut chunk would.
        if self.reads_marked() && self.inputs.evaluate(chunk, scratch, &mut evaluated).is_ok() {
            return self.rows_of(evaluated, chunk.len(), Some(kept));
        }
        self.read(&chunk.clone().settled()?, scratch)
    }

    /// Whether this aggregate can take a marked chunk, reading its arguments over every row and
    /// counting the rows the filter dropped into no group. See [`Chunk::marked`].
    ///
    /// Only the fold into an instance's own table does that, so every aggregate that goes some other
    /// way, which is a top count exchange, closed groups, a group limit, a `DISTINCT` call and the
    /// shapes read a column at a time, has the chunk cut first the way a filter always cut it.
    fn reads_marked(&self) -> bool {
        !self.alone
            && !self.sets
            && !self.compact_numeric
            && !self.distinct_count
            && !self.mixed_numeric_distinct
            && !self.radix_distinct_count
            && !(self.count_only && self.keys.len() == 1)
            && self.top_counts.is_none()
            && self.max_groups.is_none()
            && !self.closes()
            && !self.by_vector.iter().any(|&yes| yes)
            && self.calls.iter().all(|call| !call.distinct && call.affine.is_none())
    }

    /// The keys, arguments and filters out of what the expressions evaluated to over `rows` rows.
    ///
    /// For a marked chunk the keys are cut to the kept rows here, so that a row the filter dropped
    /// never opens a group, and the arguments and filters stay whole.
    fn rows_of(
        &self,
        evaluated: Vec<Vector>,
        rows: usize,
        kept: Option<&Selection>,
    ) -> Result<Rows> {
        let mut values = evaluated.into_iter();
        let mut keys: Vec<Vector> = values.by_ref().take(self.keys.len()).collect();
        let (rows, marked) = match kept {
            Some(kept) => {
                keys = Chunk::with_rows(keys, rows)?.select(kept)?.into_columns();
                (kept.len(), Some((kept.clone(), rows)))
            }
            None => (rows, None),
        };
        // An integer key a filter left as a dictionary over its page, or one still packed, is
        // opened into a flat run once for the chunk when there is more than one key. Every row is
        // hashed and then compared against a stored group once per probe step, and both read the
        // key through [`Vector::signed_at`], which on those forms works out the form, the code and
        // the value again for each row. Flat, it is an index. `GROUP BY TraficSourceID,
        // SearchEngineID, AdvEngineID` under the q40 filter is 87 thousand combinations, too many
        // for the direct map, and its probe read the three keys that way for all six hundred
        // thousand rows. One key is left alone, since the map is wide enough for most of those and
        // reads a packed page by its codes.
        if keys.len() > 1 {
            for key in &mut keys {
                if key.logical_type().is_integer()
                    && key.data().is_none()
                    && key.constant_value().is_none()
                {
                    *key = key.opened()?;
                }
            }
        }
        let mut arguments = Vec::with_capacity(self.calls.len());
        let mut filters = Vec::with_capacity(self.calls.len());
        for call in &self.calls {
            arguments.push(if call.folds() {
                values.by_ref().take(call.args.len()).collect()
            } else {
                Vec::new()
            });
            filters
                .push(call.filter.map(|_| values.next().expect("a prepared filter has a value")));
        }
        Ok(Rows { keys, arguments, filters, rows, marked })
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

    /// Whether the counted radix exchange can own this aggregate: a `COUNT(*)` grouped by one
    /// signed integer with no known range, under a TopN on the count.
    ///
    /// A key with a known range is left to the dense pass's table, which is an array indexed by the
    /// key and cheaper than any exchange.
    fn counted_top_count(&self) -> bool {
        self.count_only
            && self.top_counts.is_some()
            && self.span.is_none()
            && self.having_count.is_none()
            && self.max_groups.is_none()
            && !self.sets
            && self.constants.iter().all(Option::is_none)
            && self.keys.len() == 1
            && signed_key(self.plan.expr_type(self.keys[0]))
    }

    /// Whether every call is a plain count and the one key has a range the planner knows, so the
    /// counts can be kept in arrays the key indexes. See [`group_ranged`].
    ///
    /// Nothing that reads the groups afterwards is allowed, a TopN or a `HAVING` on the count or a
    /// limit on the groups, since those live in the table this goes around.
    fn ranged_counts(&self) -> bool {
        !self.alone
            && self.span.is_some()
            && self.top_counts.is_none()
            && self.having_count.is_none()
            && self.max_groups.is_none()
            && !self.sets
            && self.keys.len() == 1
            && self.constants.iter().all(Option::is_none)
            && signed_key(self.plan.expr_type(self.keys[0]))
            && self.calls.iter().all(|call| {
                call.folds()
                    && !call.distinct
                    && call.filter.is_none()
                    && call.returns == LogicalType::BigInt
                    && match call.name.as_str() {
                        "count_star" => call.args.is_empty(),
                        "count" => call.args.len() == 1,
                        _ => false,
                    }
            })
    }

    /// One chunk counted into this instance's arrays.
    fn count_ranged(&self, rows: &Rows, local: &mut group_ranged::Local) -> Result<Progress> {
        let rows = rows.settled()?;
        let [key] = rows.keys.as_slice() else {
            return Err(Error::internal("a ranged count received the wrong key width"));
        };
        let exchange = self.ranged.get_or_init(|| {
            let (low, values) = self.span.expect("a ranged count has a range");
            let calls = self
                .calls
                .iter()
                .map(|call| {
                    if call.args.is_empty() {
                        group_ranged::Counted::Rows
                    } else {
                        group_ranged::Counted::Valid
                    }
                })
                .collect();
            group_ranged::Exchange::new(
                self.plan.expr_type(self.keys[0]).clone(),
                i64::try_from(low).unwrap_or(i64::MIN),
                usize::try_from(values).unwrap_or(0),
                calls,
            )
        });
        let arguments: Vec<Option<&Vector>> =
            rows.arguments.iter().map(|call| call.first()).collect();
        exchange.count(key, &arguments, rows.rows, local)?;
        Ok(Progress::More)
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
        partitions: &mut [FixedRun],
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
        let before = partitions.iter().map(FixedRun::footprint).sum::<usize>();
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
        let after = partitions.iter().map(FixedRun::footprint).sum::<usize>();
        memory.grow(width_of(after.saturating_sub(before)))
    }

    fn buffer_encoded_count(
        &self,
        rows: &Rows,
        partitions: &mut [EncodedCountRun],
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
        let before = partitions.iter().map(EncodedCountRun::footprint).sum::<usize>();
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
                partitions[(hash >> shift) as usize].scatter(
                    EncodedCountRecord {
                        first: first_value,
                        second: second_value,
                        hash,
                        third: third_code,
                    },
                    EncodedCountRecord::ALL,
                );
            }
            let after = partitions.iter().map(EncodedCountRun::footprint).sum::<usize>();
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
            partitions[(hash >> shift) as usize].scatter(
                EncodedCountRecord {
                    first: first_value,
                    second: second_value,
                    hash,
                    third: third_value,
                },
                valid,
            );
        }
        let after = partitions.iter().map(EncodedCountRun::footprint).sum::<usize>();
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
        let mut local = self.partition();
        if let Some(error) = local.failure.take() {
            return Err(error);
        }
        if let Some(carried) = carried {
            self.merge(carried, &mut local)?;
        }
        while let Some(rows) = spilled.next(self)? {
            let timing = stage::Timing::start(Stage::Fold);
            let folded = self.fold(&rows, &mut local, None, None);
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
    /// How much room it takes is the one thing that turns on whether this table is the answer. An
    /// aggregate that can partition gives this table up at [`PARTITION_FROM`] groups, so room past
    /// that is room for groups it will never be asked to hold, and there is one of these per
    /// instance. [`Aggregate::ought_to_partition`] wants a second instance to have started as well,
    /// which is not knowable here, and an aggregate that says it can partition and then runs on one
    /// thread grows this table by doubling the way it always did.
    fn start(&self) -> Building {
        let held = self.alone || self.max_groups.is_some();
        self.starting(if held { Share::Whole } else { Share::Passing })
    }

    /// What one of the aggregate's shared radix partitions starts with, which is the same thing over
    /// a share of the groups.
    ///
    /// There are [`RADIX_PARTITIONS`] of these to the aggregate and the presize is a number of groups
    /// for the whole of it, so taking room for all of them in each of them takes room for the groups
    /// sixty four times over. A partition is split by hash bits and the hash spreads, so it holds
    /// about that many times fewer groups and wants room for about that many times fewer. Being
    /// wrong here costs a grow and never an answer, which is what lets the share be a division
    /// rather than a measurement.
    fn partition(&self) -> Building {
        self.starting(Share::Partition)
    }

    /// What one of an instance's own radix partitions starts with, which is what every table started
    /// with before any of this existed.
    ///
    /// There are [`RADIX_PARTITIONS`] of these per instance rather than per aggregate, so the room
    /// the one above takes is taken again once per instance here, and these are the tables the
    /// aggregate hands over as soon as [`Aggregate::cache_holds_local`] says the cache does not hold
    /// every instance's set of them. On a large group by that is almost at once, which makes this the
    /// place where room taken in advance is least likely to be room used.
    fn kept(&self) -> Building {
        self.starting(Share::Local)
    }

    fn starting(&self, share: Share) -> Building {
        let calls = self.calls.len();
        let types: Vec<_> = self.keys.iter().map(|&key| self.plan.expr_type(key).clone()).collect();
        let mut containers = self.memory.reservation();
        let mut charged = 0;
        let groups = self.presize.and_then(|groups| share.of(groups));
        // The room is reserved before the bucket array exists, so a budget that cannot hold it is
        // found out before any page of it is cleared, and the charge the first chunk makes for the
        // table is only what the table grew past it. `charged` is what the containers already hold.
        let groups = match groups {
            Some(groups) if self.reserve => {
                let room = Table::room(groups);
                (spare(&self.memory, room) && containers.grow(room).is_ok()).then(|| {
                    charged = room;
                    groups
                })
            }
            groups => groups,
        };
        let mut local = Building {
            // The keys and the rows made out of them, given back when this pass ends, because by
            // then they are in the chunks.
            scratch: self.memory.reservation(),
            // The three containers and the sets a `DISTINCT` fills, which are gone before the
            // chunks are built rather than after. Their own reservation so that their charge can go
            // when they do, which is what leaves room for the chunks. A key is not in here, because
            // a key is moved into the rows and outlives all of it. Per #272.
            containers,
            charged,
            // What the keys the table has taken a copy of own away from themselves, charged against
            // the scratch rather than against the containers because those strings move into the
            // rows and outlive the table. `charged` and this one are the same arrangement over two
            // reservations.
            charged_keys: 0,
            table: {
                let table = match groups {
                    Some(groups) => Table::with_groups(&types, groups),
                    None => Table::new(&types),
                };
                // Only where the table is the one an instance holds before it partitions. The range
                // cannot be shared the way the presize above is: a partition is split by hash bits
                // and any value can land in any of them, so a partition's array would have to cover
                // the whole range anyway, and sixty four copies of it is sixty four times the memory
                // for the same shortcut. A partition probes the buckets, which is what it did before.
                match (self.span.filter(|_| share.before_the_split()), types.as_slice()) {
                    (Some((low, values)), [ty]) => table.over_range(low, values, ty),
                    _ => table,
                }
            },
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
            coded_places: Vec::new(),
            coded_values: crate::table::Widened::default(),
            slot_runs: Vec::new(),
            coded_spent: 0,
            coded_read: 0,
            coded_map: Places::default(),
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
        // Only where the table is the one an instance holds before it partitions, for the reason
        // `span` is. A key a chunk has outside the ends builds a map of its own, as it did before,
        // so the ends being wrong costs the map and never an answer.
        let window = self
            .ends
            .filter(|_| share.before_the_split() && !self.alone)
            .and_then(|(low, values)| crate::table::seeded_window(&types, low, values));
        if let Some((low, places)) = window {
            local.coded_on.push(Origin::Window(low, places));
            local.coded_map = Places::seeded(places);
        }
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
        closed: Option<(usize, usize)>,
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
            coded_places,
            coded_values,
            slot_runs,
            coded_spent,
            coded_read,
            coded_map,
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
        // Marked rows go through the probe below as they are, and the rest of this is left to the
        // cut rows it was written for: a spill file writes a row's arguments beside its keys, and a
        // partition's rows and a closed run come here already cut.
        let settled;
        let seen_rows = if seen_rows.marked.is_some()
            && (over.is_some() || prehashed.is_some() || closed.is_some() || alone)
        {
            settled = seen_rows.settled()?;
            &*settled
        } else {
            seen_rows
        };
        let Rows { keys, arguments, filters, rows: length, marked } = seen_rows;
        let mut aside = 0;
        for at in 0..calls {
            if !self.calls[at].folds() {
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
        //
        // Emptied here and filled by whichever path below finds the slots. A key that comes in runs
        // writes each slot once as it goes, and filling the slots with `NOWHERE` first was a second
        // write of every one of them, 560 MB over seventy runs of ClickBench 28.
        slots.clear();
        // The direct map first, because a chunk it answers is a chunk that is never hashed. The
        // whole key of q1 is two dictionary codes with six combinations between them, so the map is
        // six slots long and every row after the first six is a multiply add and a load. See
        // [`Coded`](crate::table::Coded) for why that is the shape a Parquet scan hands over.
        //
        // Refused while there is a spill file, because a row that does not fit goes out whole and
        // the map has nothing to say about where it went.
        let direct = if alone
            || over.is_some()
            || closed.is_some()
            || !crate::table::fits_the_map(table.len(), *length)
        {
            coded_on.clear();
            None
        } else {
            crate::table::coded_within(keys, *length, coded_on, Some(coded_values))
        };
        // A map read by value is paid for out of the rows this table has folded. A window is the
        // caller's to choose, and a key that keeps moving past it, the way a sorted `l_orderkey`
        // does, would otherwise clear a new map of up to a quarter of a million places for every
        // chunk, in each of the radix partitions of every thread. See [`WINDOW_RATE`]. Codes a page came with are left alone,
        // since their map is as wide as the page's dictionary and no wider.
        //
        // A map that only grows upward keeps what it has and is charged for the places it gains.
        *coded_read = coded_read.saturating_add(*length);
        let grown = direct.as_ref().and_then(|codes| codes.grows(coded_on, coded_map.len()));
        let direct = direct.filter(|codes| {
            codes.same_as(coded_on)
                || !codes.reads_values()
                || coded_spent.saturating_add(codes.combos() - grown.unwrap_or(0))
                    <= coded_read.saturating_mul(WINDOW_RATE).saturating_add(WINDOW_SLACK)
        });
        let mut runs_found = false;
        if let Some(codes) = &direct {
            if !codes.same_as(coded_on) {
                if codes.reads_values() {
                    *coded_spent += codes.combos() - grown.unwrap_or(0);
                }
                codes.hold(coded_on);
                match grown {
                    // Every value keeps its place, so only the null place moves, from the last place
                    // of the old map to the last of the new one. A sorted `CounterID` grows its
                    // window a dozen times a query, and clearing the whole map for each was a
                    // second write of every place the map had, and a probe of every group again.
                    Some(span) => coded_map.widen(span, codes.combos()),
                    None => coded_map.reset(codes.combos()),
                }
            }
            // A row the map has nothing for goes through the probe and the insert every row used to
            // go through, there and then, and what comes back is written into the map before the
            // next row is looked at. A key sorted the way `CounterID` is brings each value in as a
            // run, so the first row of the run is the only one that misses and the rest of it is
            // answered by the map. Put aside for a second pass, every row of the run missed and was
            // looked at twice, which was half the rows of ClickBench 28.
            //
            // The chunk is hashed at the first miss, because a chunk the map answers whole is the
            // ordinary case once the first rows of a row group have been through. A key read by
            // value is not hashed as a chunk at all, since the rows that miss are a few dozen and
            // are hashed one at a time. `NOWHERE` is a row the group limit turned away.
            let one_at_a_time = prehashed.is_none() && codes.by_value();
            let mut hashed = false;
            let mut resolve = |row: usize| -> Result<usize> {
                let hash = match prehashed {
                    Some(prehashed) => prehashed[row],
                    None if one_at_a_time => codes.hash_of(row),
                    None => {
                        if !hashed {
                            crate::table::hash(
                                keys,
                                *length,
                                hashes,
                                crate::table::Across::OneInput,
                            );
                            hashed = true;
                        }
                        hashes[row]
                    }
                };
                match table.probe(hash, keys, row) {
                    Probe::Found(slot) => Ok(slot),
                    Probe::Vacant(_)
                        if self.max_groups.is_some_and(|limit| table.len() >= limit) =>
                    {
                        Ok(NOWHERE)
                    }
                    Probe::Vacant(bucket) => {
                        let slot = table.insert(bucket, hash, keys, row)?;
                        *groups = table.len();
                        self.fresh(states, counts, compact)?;
                        if self.sets {
                            self.fresh_seen(seen);
                        }
                        Ok(slot)
                    }
                }
            };
            // A key that comes in runs is read out of the map once a run, and the runs it was cut
            // into are the ones the aggregates fold by below. On ClickBench 28 the pass that wrote
            // every row's place, the one that read the map with it and the one that found the runs
            // again in the slots were two fifths of the fold.
            if codes.place_runs(*length, *length / RUN_ROWS, slot_runs) {
                let mut start = 0;
                for run in slot_runs.iter_mut() {
                    let (place, end) = *run;
                    let mut slot = slot_at(coded_map[place]);
                    if slot == NOWHERE {
                        slot = resolve(start)?;
                        coded_map.set(place, held_at(slot));
                    }
                    *run = (slot, end);
                    start = end;
                }
                runs_found = true;
            } else if codes.look_up(coded_map, nowhere(slots, *length)) != Some(false) {
                // One pass that finds every row's slot straight out of the map when the key is one
                // or two dictionary columns, which is the whole chunk once a row group's first rows
                // are in. Only a chunk where some row found nothing, or a key the pass does not
                // read, goes through the places and the loop here.
                codes.places(*length, coded_places);
                let mut row = 0;
                loop {
                    while row < *length {
                        let found = slot_at(coded_map[coded_places[row]]);
                        if found == NOWHERE {
                            break;
                        }
                        slots[row] = found;
                        row += 1;
                    }
                    if row == *length {
                        break;
                    }
                    let slot = resolve(row)?;
                    slots[row] = slot;
                    coded_map.set(coded_places[row], held_at(slot));
                    row += 1;
                }
            }
        } else {
            coded_on.clear();
        }
        // Runs found above leave the slots empty. See `fill_now` below.
        if !runs_found && slots.len() != *length {
            slots.clear();
            slots.resize(*length, if alone { 0 } else { NOWHERE });
        }
        // Hashed when the map did not take the chunk, since then every row is probed below.
        if !alone && closed.is_none() && direct.is_none() {
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
        // Closed groups first, which is the same run pass with the table left out. Every run between
        // `from` and `to` is a whole group, because [`interior`] found the chunk in ascending order
        // and the table is stored that way, so a key strictly inside the chunk's first and last keys
        // has no row anywhere else. Each run is a new group in a table nobody probes, and the rows
        // outside the range stay `NOWHERE` and are folded by the caller through the open table.
        if let Some((from, to)) = closed {
            crate::table::repeats(keys, *length, 0, same);
            let mut slot = NOWHERE;
            for row in from..to {
                if row == from || !same[row] {
                    slot = table.append(keys, row)?;
                    *groups = table.len();
                    self.fresh(states, counts, compact)?;
                }
                slots[row] = slot;
            }
        }
        let runs = if closed.is_none()
            && direct.is_none()
            && !alone
            && over.is_none()
            && self.max_groups.is_none()
        {
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
        while closed.is_none() && !by_run && direct.is_none() && !alone && from < *length {
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
        // The slots so far are one per kept row, and the arguments of a marked chunk are every row,
        // so each slot moves to the row it was kept from and a dropped row lands in no group. Runs
        // found above were runs of kept rows, and they move the same way, a run at a time. The
        // slots then move only if something below reads them a row at a time.
        let users = if self.count_only {
            1
        } else if self.compact_numeric {
            0
        } else {
            self.calls
                .iter()
                .enumerate()
                .filter(|&(at, call)| {
                    !self.by_vector[at] && call.folds() && !call.distinct && filters[at].is_none()
                })
                .count()
        };
        let whole;
        let length = match marked {
            Some((picks, all)) => {
                let mut spread = Vec::with_capacity(slot_runs.len() + 8);
                let most = all.saturating_mul(users) / RUN_ROWS;
                if runs_found && spread_runs(slot_runs, picks.indices(), *all, most, &mut spread) {
                    *slot_runs = spread;
                } else {
                    if runs_found {
                        fill_slots(slots, slot_runs);
                    }
                    spread_slots(slots, picks.indices(), *all);
                    runs_found = false;
                }
                whole = *all;
                &whole
            }
            None => length,
        };
        // A chunk whose runs were found out of the map has no slot a row until something below
        // reads them that way, and most chunks have nothing that does: the count and the calls
        // that fold by run read the runs. Writing a slot for every row as the runs were found was
        // a tenth of the fold of ClickBench 28. The runs are over every row by now, a marked
        // chunk's included, so they fill the slots the rows' own way round.
        let mut unfilled = runs_found;
        let mut fill_now = |slots: &mut Vec<usize>, runs: &[(usize, usize)]| {
            if std::mem::take(&mut unfilled) {
                fill_slots(slots, runs);
            }
        };
        if self.compact_numeric {
            fill_now(slots, slot_runs);
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
        // A chunk whose key the rows are sorted on lands in a handful of groups one after another,
        // and then each aggregate takes a run of rows into one group at once rather than a row at
        // a time. The pass that finds out is one compare a row, so it is only taken where the
        // slots can come in runs at all, which is a key whose rows are grouped together.
        //
        // How many of this chunk's calls would read the runs decides whether finding them pays, so
        // it was counted above and is handed to `slot_runs_of` as its budget. The count loop below
        // reads them once, and a call that goes by vector, that does not fold, that is `DISTINCT` or
        // that carries a `FILTER` never reaches the run path at all.
        let by_runs = runs_found || slot_runs_of(slots, slot_runs, users);
        if self.count_only {
            if by_runs {
                let mut start = 0;
                for &(slot, end) in slot_runs.iter() {
                    if slot != NOWHERE {
                        counts[slot] += (end - start) as i64;
                    }
                    start = end;
                }
            } else {
                fill_now(slots, slot_runs);
                for &slot in slots.iter() {
                    if slot != NOWHERE {
                        counts[slot] += 1;
                    }
                }
            }
        }
        // The aggregate half of #61. Every call that is not `DISTINCT` folds the whole chunk in one
        // pass, with the aggregate and the layout of its argument matched on once for the chunk
        // rather than once per row, and with no `Value` built at all on the paths the kernel covers.
        // How many rows land in each group, taken the first time a call can use it. See
        // [`rudb_kernels::group_tally`].
        let mut counted: Option<Option<Vec<i64>>> = None;
        // The calls that would each walk the runs on their own, folded in one walk instead. What a run
        // costs before a value of it is read is most of what a run costs at all, and it was paid once
        // per call. Asked again with what it left, because a pass covers one layout and q01 has two.
        // See [`rudb_kernels::update_shared_runs`].
        let mut shared = 0_u64;
        if by_runs && users > 1 && !self.count_only && !self.compact_numeric {
            let offered = self
                .calls
                .iter()
                .enumerate()
                .filter(|&(at, call)| {
                    !self.by_vector[at] && call.folds() && !call.distinct && filters[at].is_none()
                })
                .fold(0_u64, |offered, (at, _)| offered | one_call(at));
            let inputs: Vec<Option<&Vector>> =
                arguments.iter().map(|argument| argument.first()).collect();
            loop {
                let took = update_shared_runs(
                    states,
                    slot_runs,
                    calls,
                    &inputs,
                    offered & !shared,
                    *length,
                )?;
                if took == 0 {
                    break;
                }
                shared |= took;
            }
        }
        for (at, call) in self.calls.iter().enumerate() {
            if self.count_only || self.compact_numeric {
                break;
            }
            if self.by_vector[at] || !call.folds() {
                continue;
            }
            if shared & one_call(at) != 0 {
                continue;
            }
            if call.distinct {
                fill_now(slots, slot_runs);
                aside += self.distinct(states, seen, seen_rows, slots, at, given)?;
                continue;
            }
            if by_runs
                && filters[at].is_none()
                && update_runs(states, slot_runs, calls, at, arguments[at].first(), *length)?
            {
                continue;
            }
            fill_now(slots, slot_runs);
            let (picked, tallied) = match &filters[at] {
                None => {
                    let rows = slots.len().min(*length);
                    let groups = states.len().checked_div(calls).unwrap_or(usize::MAX);
                    let tally = counted.get_or_insert_with(|| group_tally(&slots[..rows], groups));
                    (&*slots, tally.as_deref())
                }
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
                    (&*kept, None)
                }
            };
            if !update_general(states, picked, calls, at, &arguments[at], *length)? {
                let argument = arguments[at].first();
                update_tallied(states, picked, tallied, calls, at, argument, *length)?;
            }
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
            None if !alone && closed.is_none() && crowded(&self.memory) => {
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
            // A bound that is not below the group count cannot throw a group away, and the loop
            // below arrives back at every slot in slot order after a whole insertion sort to get
            // there. Worse than the sort is what naming every slot costs afterwards: a named slot
            // sends each key column out through a `Value` apiece where an unnamed run goes through
            // [`Table::column`], which copies the run as it is stored, so a VARCHAR key is an
            // allocation per group rather than one copy of the block.
            //
            // Partitioning is what makes this the ordinary case rather than a corner. The bound is
            // compared against one partition's groups, so a split by [`RADIX_PARTITIONS`] leaves
            // each of them holding that many times fewer, and a bound that was well under the group
            // count before the split is well over it after. ClickBench 39 has 5445 groups under a
            // bound of 1010 and eighty five of them to a partition.
            (Some((bound, _)), _) if bound >= groups => None,
            (Some((bound, ranks)), _) => {
                let mut best = largest(groups, bound, |slot| count(slot, ranks));
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
        // The groups of one chunk, as the slots they are stored at, which is what the run at a time
        // finish below is handed. A selection already holds them in that form and a chunk that was
        // not selected from is a range, written out here once per chunk rather than once per call.
        let mut run: Vec<usize> = Vec::new();
        // row at a time: the outer loop steps a chunk at a time and the key columns are copied a
        // column at a time out of the table, so the only thing left here that is per group is asking
        // each accumulator for its result, which is 2g (#61).
        for start in (0..output_groups).step_by(VECTOR_SIZE) {
            let end = (start + VECTOR_SIZE).min(output_groups);
            let slots = selected.as_ref().map(|slots| &slots[start..end]);
            let picked: &[usize] = match slots {
                Some(slots) => slots,
                None => {
                    run.clear();
                    run.extend(start..end);
                    &run
                }
            };
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
                // The run at a time finish, for a call whose state and output column agree on a
                // shape it covers. It writes the whole column in one pass with no `Value` per group,
                // where the loop below asks each accumulator for a `Value`, pushes it into a run of
                // values, pushes every one of them again into the vector's flat data, reads them all
                // back to build the mask and then drops them. `spec/perf/14-what-a-group-costs.md`
                // measures that round trip as the largest single thing an added aggregate call
                // costs, at 0.479 G of the 3.248 G two added calls spend on TPC-H SF1.
                //
                // Everything it does not cover falls through unchanged, which is a `min` or a `max`,
                // whose state owns a value away from itself, an affine call and a sum read off a
                // mean, which finish differently from the state they read, and the two compact
                // shapes, which have no accumulators. A call that repeats an earlier one finishes
                // exactly the way that one does, so it comes through here and the only difference is
                // the state it is pointed at.
                let held = self.calls[at].state_of(at);
                if !self.count_only && !self.compact_numeric && self.calls[at].finishes_plainly() {
                    if let Some(vector) = finish_run(&states, picked, calls, held, ty)? {
                        columns.push(vector);
                        continue;
                    }
                }
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
                        match (self.calls[at].affine, self.calls[at].reads_total) {
                            (Some((source, offset)), _) => states[slot * calls + source]
                                .finish_offset(offset, affine_rows[source]),
                            (None, Some(source)) => sum_from_mean(
                                &states[slot * calls + source],
                                &self.calls[at].returns,
                            ),
                            // `held` is this call's own state, and the earlier call's where this one
                            // repeats it.
                            (None, None) => states[slot * calls + held].finish(),
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
                let into = table.get_or_insert_with(|| self.partition());
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
                let waiting = carried.get_or_insert_with(|| self.partition());
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
    ///
    /// # A pushed down bound used to raise the line and no longer does
    ///
    /// A count descending TopN pushes its bound down here, and the bound is applied when a table is
    /// finished, so a split applies it once per partition and hands the pipeline above that many
    /// times as many rows. This asked for [`RADIX_PARTITIONS`] times the bound before it would
    /// split, which is the group count at which each partition would still hold more groups than
    /// the bound and the reduction would go back to being worth what it was.
    ///
    /// That threshold cost more than it saved. ClickBench 40 groups five columns under a bound of
    /// 1010, and at ten million rows it has 40,306 groups against a threshold of 64,640, so no
    /// instance could reach the line at any thread count and the aggregate merged instead of
    /// splitting. Measured on a 32 thread i9-13900K against a ten million row native table, the
    /// query ran in 42.2 ms and in 26.1 ms once the threshold went, which is 1.62 times. Nothing
    /// else in the suite moved: 34, 35, 37, 38 and 39 answer through the encoded count path and
    /// never reach this decision at all.
    ///
    /// Removing the threshold on its own is a wash, 42.2 ms to 40.1. What makes it pay is the
    /// companion change in [`Aggregate::finishing`], which stops a partition whose groups all fit
    /// under the bound from naming every slot and sending its keys out through a `Value` apiece.
    /// Those two together take the aggregate's emit from 17.8 ms of CPU to 1.7, and that is the
    /// difference. #486 has the rest of the measurement.
    ///
    /// The extra rows the pipeline above now gets were priced when a row the TopN rejected still
    /// built a Vec for its key and a Vec for its whole payload. #1108 made a losing row cost one
    /// comparison and no allocation, and #1111 and #1123 stopped the TopN decoding strings, so they
    /// are much cheaper than they were when the threshold was written.
    fn ought_to_partition(&self, table: &Building) -> bool {
        !self.alone
            && self.max_groups.is_none()
            && self.started.load(Ordering::Relaxed) > 1
            && (table.groups >= self.partition_from || crowded(&self.memory))
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

    /// The chunks put aside by [`Aggregate::open`], joined and split among the partitions.
    ///
    /// A chunk split sixty four ways leaves each partition a few dozen rows, and the fold of a few
    /// dozen rows is mostly the fold's own fixed cost: a gathered copy of every column, a probe
    /// setup, and a table to reach. On ClickBench 29 that made the multi threaded aggregate cost
    /// sixty percent more CPU than the same query on one thread. Joining [`GATHER_ROWS`] rows first
    /// gives each partition a few hundred rows a fold, which is what the fold is sized for.
    ///
    /// Columns that cannot be joined into one flat vector, which a dictionary from another page is,
    /// leave the chunks to be split one at a time the way they always were.
    fn drain(
        &self,
        folded: u64,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        if spreading.pending.is_empty() {
            return Ok(());
        }
        let pieces = std::mem::take(&mut spreading.pending);
        spreading.gathered = 0;
        for rows in Rows::joined(pieces)? {
            if self.locally.load(Ordering::Relaxed) && self.still_local(folded, spreading, own)? {
                self.spread_own(&rows, spreading, own)?;
            } else {
                self.spread(&rows, spreading)?;
            }
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
            let table = own[partition].get_or_insert_with(|| self.kept());
            if let Some(error) = table.failure.take() {
                return Err(error);
            }
            self.fold(selected, table, Some(&spreading.keyed[partition]), None)?;
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
                let into = own[at].get_or_insert_with(|| self.kept());
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
                    let table = held.table.get_or_insert_with(|| self.partition());
                    self.fold(selected, table, Some(&keyed[partition]), None)?;
                }
                Err(TryLockError::WouldBlock) => waiting.push(partition),
                Err(TryLockError::Poisoned(error)) => return Err(poisoned(error)),
            }
        }
        for &partition in waiting.iter() {
            let selected =
                ready[partition].as_ref().expect("only a filled partition was put aside");
            let mut held = self.merged[partition].lock().map_err(poisoned)?;
            let table = held.table.get_or_insert_with(|| self.partition());
            self.fold(selected, table, Some(&keyed[partition]), None)?;
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
        if let Some(template) = &self.template {
            states.extend_from_slice(template);
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

    /// The groups between `from` and `to` of a sorted chunk, answered as a chunk of their own.
    ///
    /// Each run of the key in there is a whole group, for the reason [`interior`] gives, so its
    /// answer is known as soon as the run ends. Going through the table cost a copy of the key a row
    /// at a time and a fresh accumulator per call for every group, and then a probe of the slots to
    /// fold each row into them, and on `GROUP BY l_orderkey` over lineitem, which is four rows a
    /// group, that was most of the instructions of the grouping. Here the key is gathered once at
    /// the first row of every run, and each call is one pass that adds a run up where it lies.
    ///
    /// `None` when an argument is in a layout this does not read, before anything is built, so the
    /// caller can hand the chunk to the table instead.
    fn close_runs(&self, rows: &Rows, from: usize, to: usize) -> Result<Option<Chunk>> {
        let mut same = Vec::new();
        crate::table::repeats(&rows.keys, rows.rows, 0, &mut same);
        let mut starts: Vec<u32> = Vec::new();
        for (row, &repeat) in same.iter().enumerate().take(to).skip(from) {
            if row == from || !repeat {
                starts.push(u32::try_from(row).map_err(|_| Error::internal("a chunk too long"))?);
            }
        }
        let groups = starts.len();
        let end = |group: usize| starts.get(group + 1).map_or(to, |&start| start as usize);
        let types = self.schema.types();
        let width = self.groups.len();
        let mut columns = Vec::with_capacity(types.len());
        let mut key = 0;
        for (at, ty) in types.iter().take(width).enumerate() {
            if let Some(value) = &self.constants[at] {
                columns.push(Vector::constant(ty.clone(), value.clone(), groups));
            } else {
                columns.push(rows.keys[key].gather(&starts)?);
                key += 1;
            }
        }
        let mut answers = vec![0_i128; groups];
        let mut valid = vec![true; groups];
        for (at, ty) in types.iter().skip(width).enumerate() {
            valid.fill(true);
            if self.calls[at].name == "count_star" {
                for (group, &start) in starts.iter().enumerate() {
                    answers[group] = (end(group) - start as usize) as i128;
                }
            } else {
                let argument = rows.arguments[at][0].clone().into_flat()?;
                let nulls = argument.validity().has_nulls(rows.rows).then(|| argument.validity());
                let counting = self.calls[at].name == "count";
                let values = match counting {
                    true => None,
                    false => match integers(&argument, rows.rows) {
                        Some(values) => Some(values),
                        None => return Ok(None),
                    },
                };
                for (group, &start) in starts.iter().enumerate() {
                    let run = start as usize..end(group);
                    let (total, seen) = match (&values, nulls) {
                        (None, None) => (run.len() as i128, true),
                        (None, Some(nulls)) => {
                            (run.filter(|&row| nulls.is_valid(row)).count() as i128, true)
                        }
                        (Some(values), None) => {
                            (values[run].iter().map(|&v| i128::from(v)).sum(), true)
                        }
                        (Some(values), Some(nulls)) => {
                            let mut total = 0_i128;
                            let mut seen = false;
                            for row in run {
                                if nulls.is_valid(row) {
                                    total += i128::from(values[row]);
                                    seen = true;
                                }
                            }
                            (total, seen)
                        }
                    };
                    answers[group] = total;
                    valid[group] = seen;
                }
            }
            columns.push(whole_answers(&answers, &valid, ty)?);
        }
        Chunk::with_rows(columns, groups).map(Some)
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

/// `slots` as `rows` places of `NOWHERE`, for a path that fills them in any order.
fn nowhere(slots: &mut Vec<usize>, rows: usize) -> &mut [usize] {
    slots.clear();
    slots.resize(rows, NOWHERE);
    slots
}

/// Every row's slot, out of runs of one slot each and the row each ends before.
fn fill_slots(slots: &mut Vec<usize>, runs: &[(usize, usize)]) {
    slots.clear();
    for &(slot, end) in runs {
        slots.resize(end, slot);
    }
}

/// Moves the slot of each kept row to the row it was kept from, `all` rows long, and puts every
/// row the filter dropped in no group.
///
/// Backwards and in place. The kept rows ascend and each is at least its own place in the list, so
/// a slot is always read before anything is written over it.
fn spread_slots(slots: &mut Vec<usize>, kept: &[u32], all: usize) {
    slots.resize(all, NOWHERE);
    for (place, &row) in kept.iter().enumerate().rev() {
        let slot = std::mem::replace(&mut slots[place], NOWHERE);
        slots[row as usize] = slot;
    }
}

/// The runs of a marked chunk's kept rows as runs of all `all` of its rows, with every row the
/// filter dropped in a run of `NOWHERE`, or `false` with `into` empty once that takes more than
/// `most` runs.
///
/// A run of kept rows with no dropped row inside it is one run of rows, which the first and last
/// kept row say without looking at the ones between. That is nearly every run of a filter that
/// drops few rows. Spreading the slots and cutting them into runs again took two passes over every
/// row for the same answer, a third of the fold of ClickBench 28.
fn spread_runs(
    runs: &[(usize, usize)],
    kept: &[u32],
    all: usize,
    most: usize,
    into: &mut Vec<(usize, usize)>,
) -> bool {
    into.clear();
    let mut push = |slot: usize, end: usize| {
        match into.last_mut() {
            Some(last) if last.0 == slot => last.1 = end,
            _ => into.push((slot, end)),
        }
        into.len() <= most
    };
    let mut from = 0;
    let mut row = 0;
    for &(slot, end) in runs {
        let Some(within) = kept.get(from..end) else { return false };
        let mut rest = within;
        while let (Some(&first), Some(&last)) = (rest.first(), rest.last()) {
            let (first, last) = (first as usize, last as usize);
            let length = if last - first == rest.len() - 1 {
                rest.len()
            } else {
                1 + rest.windows(2).take_while(|pair| pair[1] == pair[0] + 1).count()
            };
            let upto = first + length;
            if (first > row && !push(NOWHERE, first)) || !push(slot, upto) {
                into.clear();
                return false;
            }
            row = upto;
            rest = &rest[length..];
        }
        from = end;
    }
    if from != kept.len() || row > all || (row < all && !push(NOWHERE, all)) {
        into.clear();
        return false;
    }
    true
}

/// One chunk of rows, in the vectors the row loop reads them out of.
///
/// The same shape whether the rows came from the operator below or from a spill file, which is what
/// lets one loop serve both.
#[derive(Clone, Debug)]
struct Rows {
    keys: Vec<Vector>,
    arguments: Vec<Vec<Vector>>,
    filters: Vec<Option<Vector>>,
    rows: usize,
    /// For a marked chunk, the rows of the arguments and filters the keys stand for and how many
    /// rows the arguments and filters have. The keys are `rows` long either way. See
    /// [`Aggregate::reads_marked`].
    marked: Option<(Selection, usize)>,
}

impl Rows {
    /// The same rows with the arguments and filters cut to the kept rows, the way they would have
    /// come had the filter cut the chunk. Rows that were not marked are handed back as they are.
    fn settled(&self) -> Result<Cow<'_, Self>> {
        let Some((kept, _)) = &self.marked else { return Ok(Cow::Borrowed(self)) };
        let cut = |vector: &Vector| vector.gather(kept.indices());
        Ok(Cow::Owned(Self {
            keys: self.keys.clone(),
            arguments: self
                .arguments
                .iter()
                .map(|call| call.iter().map(cut).collect::<Result<_>>())
                .collect::<Result<_>>()?,
            filters: self
                .filters
                .iter()
                .map(|filter| filter.as_ref().map(cut).transpose())
                .collect::<Result<_>>()?,
            rows: self.rows,
            marked: None,
        }))
    }

    /// The rows from `at` for `len`, every column cut the same way.
    fn slice(&self, at: usize, len: usize) -> Result<Self> {
        let cut = |vector: &Vector| vector.slice(at, len);
        Ok(Self {
            keys: self.keys.iter().map(cut).collect::<Result<_>>()?,
            arguments: self
                .arguments
                .iter()
                .map(|call| call.iter().map(cut).collect::<Result<_>>())
                .collect::<Result<_>>()?,
            filters: self
                .filters
                .iter()
                .map(|filter| filter.as_ref().map(cut).transpose())
                .collect::<Result<_>>()?,
            rows: len,
            marked: None,
        })
    }

    /// How many columns one of these rows is written out as, which is
    /// [`Aggregate::spilled_types`] long.
    fn width(&self) -> usize {
        self.keys.len()
            + self.arguments.iter().map(Vec::len).sum::<usize>()
            + self.filters.iter().flatten().count()
    }

    /// The pieces as one batch when every column of them can be laid end to end, and as they came
    /// when one of them cannot.
    fn joined(pieces: Vec<Self>) -> Result<Vec<Self>> {
        if pieces.len() < 2 {
            return Ok(pieces);
        }
        let first = &pieces[0];
        let mut keys = Vec::with_capacity(first.keys.len());
        for at in 0..first.keys.len() {
            let Some(laid) = lay(&pieces, |piece| piece.keys.get(at))? else { return Ok(pieces) };
            keys.push(laid);
        }
        let mut arguments = Vec::with_capacity(first.arguments.len());
        for (call, columns) in first.arguments.iter().enumerate() {
            let mut laid_call = Vec::with_capacity(columns.len());
            for at in 0..columns.len() {
                let column = |piece| Self::argument(piece, call, at);
                let Some(laid) = lay(&pieces, column)? else { return Ok(pieces) };
                laid_call.push(laid);
            }
            arguments.push(laid_call);
        }
        let mut filters = Vec::with_capacity(first.filters.len());
        for (call, filter) in first.filters.iter().enumerate() {
            if filter.is_none() {
                if pieces.iter().any(|piece| piece.filters.get(call).is_none_or(Option::is_some)) {
                    return Ok(pieces);
                }
                filters.push(None);
                continue;
            }
            let column = |piece| Self::filter(piece, call);
            let Some(laid) = lay(&pieces, column)? else { return Ok(pieces) };
            filters.push(Some(laid));
        }
        let rows = pieces.iter().map(|piece| piece.rows).sum();
        Ok(vec![Self { keys, arguments, filters, rows, marked: None }])
    }

    fn argument(&self, call: usize, at: usize) -> Option<&Vector> {
        self.arguments.get(call)?.get(at)
    }

    fn filter(&self, call: usize) -> Option<&Vector> {
        self.filters.get(call)?.as_ref()
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
            marked: None,
        })
    }
}

/// One column of every piece laid end to end, or `None` when a piece lacks it or it has no flat
/// layout.
fn lay<'a>(
    pieces: &'a [Rows],
    column: impl Fn(&'a Rows) -> Option<&'a Vector>,
) -> Result<Option<Vector>> {
    let mut columns = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let Some(vector) = column(piece) else { return Ok(None) };
        columns.push(vector);
    }
    rudb_vector::concat(columns[0].logical_type(), &columns)
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
    counted: group_count::Local,
    ranged: group_ranged::Local,
    grouped_distinct: group_distinct::Local,
    encoded: bool,
    encoded_records: Vec<EncodedCountRun>,
    encoded_memory: Reservation,
    radix_distinct: bool,
    radix_distinct_records: Vec<BigIntDistinctPartition>,
    radix_distinct_memory: Reservation,
    fixed: bool,
    fixed_records: Vec<FixedRun>,
    fixed_memory: Reservation,
    fixed_blocks: FixedBlocks,
    dense: bool,
    dense_codes: Vec<Blocks<u32>>,
    dense_nulls: i64,
    dense_memory: Reservation,
    /// The table this instance folds into while it still keeps its groups to itself.
    ///
    /// Every instance starts with one, because splitting a chunk sixty four ways is not free and a
    /// small aggregate never earns it back. It goes when [`Aggregate::ought_to_partition`] says the
    /// table has grown enough to be worth sharing, and from then on this is `None` and the chunks go
    /// straight into the partitions.
    single: Option<Building>,
    /// The groups this instance closed, which skip `single` and the partitions and are finished into
    /// chunks when the instance combines. `None` until the first chunk that closes one.
    closed: Option<Building>,
    /// The groups this instance closed straight from their runs, already answered, and the room
    /// they take. See [`Aggregate::close_runs`].
    ran: Vec<Chunk>,
    ran_memory: Reservation,
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
    /// Chunks put aside until there are [`GATHER_ROWS`] rows of them to split at once, and how many
    /// rows that is. See [`Aggregate::drain`].
    pending: Vec<Rows>,
    gathered: usize,
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
            pending: Vec::new(),
            gathered: 0,
        }
    }
}

/// Whether a total of `argument` declared as `returns` is the sum of the raw integers the argument
/// holds and nothing else, which is what [`Aggregate::close_runs`] adds up.
///
/// A decimal counts only at the scale the total is declared at, since any other needs a rescale per
/// row, and only up to eighteen digits, which is what fits in the `i64` it is read as.
fn whole_total(argument: &LogicalType, returns: &LogicalType) -> bool {
    use LogicalType as T;
    match (argument, returns) {
        (T::Decimal { width, scale }, T::Decimal { scale: declared, .. }) => {
            *width <= 18 && scale == declared
        }
        (
            T::TinyInt
            | T::SmallInt
            | T::Integer
            | T::BigInt
            | T::UTinyInt
            | T::USmallInt
            | T::UInteger,
            T::TinyInt
            | T::SmallInt
            | T::Integer
            | T::BigInt
            | T::HugeInt
            | T::UTinyInt
            | T::USmallInt
            | T::UInteger
            | T::UBigInt
            | T::UHugeInt,
        ) => true,
        _ => false,
    }
}

/// A flat column of whole numbers read as `i64`, the raw integers of a decimal included.
///
/// `None` for any other layout, which leaves the chunk to the table.
fn integers(flat: &Vector, rows: usize) -> Option<Vec<i64>> {
    macro_rules! widened {
        ($values:expr) => {{
            let values = $values.as_slice();
            values.get(..rows)?.iter().map(|&value| i64::from(value)).collect()
        }};
    }
    Some(match flat.data() {
        Some(Data::Int8(values)) => widened!(values),
        Some(Data::Int16(values)) => widened!(values),
        Some(Data::Int32(values)) => widened!(values),
        Some(Data::Int64(values)) => widened!(values),
        Some(Data::UInt8(values)) => widened!(values),
        Some(Data::UInt16(values)) => widened!(values),
        Some(Data::UInt32(values)) => widened!(values),
        _ => return None,
    })
}

/// Where the closed groups of a chunk start and end, as the first row after the first run of the
/// key and the first row of its last run.
///
/// `None` unless the key is a run of integers in ascending order with no null, in a form where two
/// rows are compared by what they hold. A chunk that goes down anywhere is refused whole, which is
/// what makes a stale promise about the table's order cost time rather than a wrong answer: a group
/// is only closed out of a chunk that is sorted, and a table that is sorted puts every row of a key
/// strictly inside a sorted chunk inside that chunk. Two runs or fewer have nothing strictly inside.
///
/// The forms are the ones [`crate::table::repeats`] compares exactly, in the order it tries them,
/// because the fold finds the runs with it and a run it split would be one group closed twice.
fn interior(key: &Vector, rows: usize) -> Option<(usize, usize)> {
    if rows < 3 || key.validity().has_nulls(rows) {
        return None;
    }
    fn bounds<T: PartialOrd>(rows: usize, at: impl Fn(usize) -> T) -> Option<(usize, usize)> {
        let (mut from, mut to) = (0, 0);
        let mut before = at(0);
        for row in 1..rows {
            let value = at(row);
            if value < before {
                return None;
            }
            if value != before {
                if from == 0 {
                    from = row;
                }
                to = row;
            }
            before = value;
        }
        (from > 0 && from < to).then_some((from, to))
    }
    macro_rules! flat {
        ($values:expr) => {{
            let values = $values.as_slice();
            if values.len() < rows {
                return None;
            }
            bounds(rows, |row| values[row])
        }};
    }
    // A packed code is the value less the frame's base, so codes are in the order the values are.
    if let Some(packed) = key.packed_parts() {
        return bounds(rows, |row| packed.code(row));
    }
    match key.data()? {
        Data::Int8(values) => flat!(values),
        Data::Int16(values) => flat!(values),
        Data::Int32(values) => flat!(values),
        Data::Int64(values) => flat!(values),
        Data::UInt8(values) => flat!(values),
        Data::UInt16(values) => flat!(values),
        Data::UInt32(values) => flat!(values),
        Data::UInt64(values) => flat!(values),
        _ => None,
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
    /// What `coded_map` was filled against, which is what says it still means anything.
    ///
    /// Empty when the last chunk was not one the direct map could answer, so the map is rebuilt
    /// rather than read. See [`Coded`](crate::table::Coded).
    coded_on: Vec<Origin>,
    /// One slot per combination of codes, or [`UNSEEN`](crate::table::UNSEEN) where that
    /// combination has not been seen.
    coded_map: Places,
    /// Which combination each row of the last chunk is, worked out one key column at a time.
    coded_places: Vec<usize>,
    /// The values of each integer key column the map reads by value, widened, and their runs.
    coded_values: crate::table::Widened,
    /// The last chunk's slots cut into runs of one slot, each its slot and the row it ends before,
    /// when they came in runs long enough to fold a run at a time.
    slot_runs: Vec<(usize, usize)>,
    /// How many places the maps built on a window of values have cleared, summed over every build.
    coded_spent: usize,
    /// How many rows this table has folded, which is what pays for a map read by value.
    coded_read: usize,
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
        Ok(Some(Rows { keys, arguments, filters, rows, marked: None }))
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

    /// A row costs a grouped aggregate far more than it costs the operators it sits above.
    ///
    /// It builds the row's key, hashes it, probes a table with the hash, and compares the key
    /// against whatever it landed on, and only then does it fold the row's values into a state.
    /// None of that is the one load and one store an ordinary operator spends, and the probe is a
    /// random access into a table that a query with many groups has no hope of keeping in cache.
    ///
    /// What is counted is the key, because the key is what all four of those steps are about and
    /// it is the one thing about the cost that is known before a row is read. A column of it is
    /// worth one and a variable width column of it is worth three more, since a `VARCHAR` key is
    /// hashed over its bytes rather than over a register and compared the same way. Measured on
    /// ClickBench 39, whose key is three narrow integers beside two wide strings, the aggregate
    /// spends a hundred and eleven nanoseconds a row against the two or three a plain projection
    /// does, and this counts it as eleven.
    ///
    /// The key and the arguments are worked out here rather than by a projection underneath, so
    /// the steps that compute them are counted too. ClickBench 42 groups by
    /// `DATE_TRUNC('minute', EventTime)` and there is no `Project` in its pipeline at all, because
    /// the truncation is a step of this operator's own expressions.
    ///
    /// An ungrouped aggregate over bare columns has no key, no table and nothing to compute, so it
    /// answers zero and the rule above the scan is left exactly as it was.
    fn weight(&self) -> usize {
        let keys = self.keys.iter().map(|&key| {
            let ty = self.plan.expr_type(key);
            if ty.physical() == PhysicalType::Varlen || ty.is_nested() { 4 } else { 1 }
        });
        keys.sum::<usize>() + self.inputs.passes()
    }

    fn local(&self) -> Partitioned {
        self.started.fetch_add(1, Ordering::Relaxed);
        Partitioned {
            mixed: group_mixed::Local::new(&self.memory),
            counted: group_count::Local::new(&self.memory),
            ranged: group_ranged::Local::new(&self.memory),
            grouped_distinct: group_distinct::Local::new(&self.memory),
            encoded: false,
            encoded_records: (0..RADIX_PARTITIONS).map(|_| EncodedCountRun::default()).collect(),
            encoded_memory: self.memory.reservation(),
            radix_distinct: false,
            radix_distinct_records: (0..RADIX_PARTITIONS)
                .map(|_| BigIntDistinctPartition::default())
                .collect(),
            radix_distinct_memory: self.memory.reservation(),
            fixed: false,
            fixed_records: (0..RADIX_PARTITIONS).map(|_| FixedRun::default()).collect(),
            fixed_blocks: FixedBlocks::default(),
            fixed_memory: self.memory.reservation(),
            dense: false,
            dense_codes: (0..DENSE_PARTITIONS).map(|_| Blocks::default()).collect(),
            dense_nulls: 0,
            dense_memory: self.memory.reservation(),
            single: Some(self.start()),
            closed: None,
            ran: Vec::new(),
            ran_memory: self.memory.reservation(),
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
            counted,
            ranged,
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
            closed,
            ran,
            ran_memory,
            installed,
            expressions,
            spreading,
            own,
            folded,
        } = local;
        let rows = self.read(chunk, expressions)?;
        if self.ranged_counts() {
            return self.count_ranged(&rows, ranged);
        }
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
        if self.counted_top_count() {
            let [key] = rows.keys.as_slice() else {
                return Err(Error::internal(
                    "a counted radix exchange received the wrong key width",
                ));
            };
            group_count::Exchange::buffer(
                &self.counted,
                self.plan.expr_type(self.keys[0]),
                key,
                rows.rows,
                counted,
            )?;
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
                    let before = dense_codes.iter().map(Blocks::footprint).sum::<usize>();
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
                    let after = dense_codes.iter().map(Blocks::footprint).sum::<usize>();
                    dense_memory.grow(width_of(after.saturating_sub(before)))?;
                    *dense = true;
                    return Ok(Progress::More);
                }
            }
        }
        // The groups strictly inside the chunk are closed and skip the table, and only the first
        // and the last run go the ordinary way, since either of them can carry on into a chunk some
        // other instance holds. The two ends are cut before anything is folded, so a vector that
        // cannot be cut leaves the whole chunk to the ordinary path.
        if self.closes() {
            if let Some((from, to)) = interior(&rows.keys[0], rows.rows) {
                if let (Ok(head), Ok(tail)) = (rows.slice(0, from), rows.slice(to, rows.rows - to))
                {
                    if self.closes_by_run() {
                        let timing = stage::Timing::start(Stage::Fold);
                        let answered = self.close_runs(&rows, from, to);
                        timing.stop(0);
                        if let Some(answered) = answered? {
                            ran_memory.grow(width_of(answered.footprint()))?;
                            ran.push(answered);
                            self.open(&head, single, installed, spreading, own, folded)?;
                            self.open(&tail, single, installed, spreading, own, folded)?;
                            return Ok(Progress::More);
                        }
                    }
                    let building = closed.get_or_insert_with(|| self.shut());
                    let timing = stage::Timing::start(Stage::Fold);
                    let done = self.fold(&rows, building, None, Some((from, to)));
                    timing.stop(0);
                    done?;
                    self.open(&head, single, installed, spreading, own, folded)?;
                    self.open(&tail, single, installed, spreading, own, folded)?;
                    return Ok(Progress::More);
                }
            }
        }
        self.open(&rows, single, installed, spreading, own, folded)?;
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
            counted,
            ranged,
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
            closed,
            mut ran,
            ran_memory,
            mut spreading,
            mut own,
            folded,
            ..
        } = local;
        self.drain(folded, &mut spreading, &mut own)?;
        if !ran.is_empty() {
            let mut built = self.built.lock().map_err(poisoned)?;
            built.chunks.append(&mut ran);
            built.held.push(ran_memory);
        }
        // Closed groups are finished groups, so they become chunks here on this instance's thread
        // and wait beside the answer for the partitions to finish.
        if let Some(closed) = closed {
            if let Some(error) = closed.failure {
                return Err(error);
            }
            let mut chunks = Vec::new();
            let mut held = self.memory.reservation();
            if self.finish(closed, &mut chunks, &mut held)?.is_some() {
                return Err(Error::internal("a closed group went to a spill file"));
            }
            let mut built = self.built.lock().map_err(poisoned)?;
            built.chunks.append(&mut chunks);
            built.held.push(held);
        }
        if mixed.used() {
            let state = self.mixed.get().expect("a mixed exchange exists after its sink");
            state.combine(mixed)?;
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if ranged.used() {
            let state = self.ranged.get().expect("a ranged exchange exists after its sink");
            state.combine(ranged)?;
            self.built.lock().map_err(poisoned)?.instances += 1;
            return Ok(());
        }
        if counted.used() {
            let state = self.counted.get().expect("a counted exchange exists after its sink");
            state.combine(counted)?;
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
                if rows.is_empty() {
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
            // The smaller table goes into the larger, because every group of the one merged in is
            // a probe and every group it has that the other lacks is an insert and a fresh set of
            // accumulators, while the groups of the one kept cost nothing. Instances that read
            // different stretches of a sorted key hold mostly different groups, and on ClickBench
            // 28 at six threads the merge was 121 of 1427 samples with a third of it inserts.
            let (from, mut into) = if waiting.groups > arriving.groups {
                (arriving, waiting)
            } else {
                (waiting, arriving)
            };
            self.merge(from, &mut into)?;
            arriving = into;
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
        if let Some(ranged) = self.ranged.get() {
            let chunks = ranged.finish(&self.memory)?;
            return self.out.fill(chunks);
        }
        if let Some(counted) = self.counted.get() {
            let bound = self.top_counts.expect("a counted exchange has a TopN bound").0;
            let degree = fixed_degree(counted.records()?, threads);
            let chunks = counted.finish(threads, degree, bound, &self.memory)?;
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
                        .map(|runs| runs.runs.iter().map(EncodedCountRun::len).sum::<usize>())
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
            let input = fixed
                .partitions
                .iter()
                .map(|partition| {
                    partition
                        .lock()
                        .map(|runs| runs.runs.iter().map(|run| run.rows.len()).sum::<usize>())
                        .map_err(poisoned)
                })
                .sum::<Result<usize>>()?;
            let degree = fixed_degree(input, threads);
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
                            self.top_counts.map(|(bound, _)| bound),
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

/// Which split of `splits` an encoded count record belongs to.
///
/// The record carries thirty two bits of hash. The bucket tag reads the top eight and a table sized
/// for one split reads at most the low sixteen, so the split takes the eight between, and a
/// partition too large for 256 splits of [`FIXED_SPLIT_ROWS`] gets larger tables instead.
#[inline]
fn encoded_split(hash: u32, splits: usize) -> usize {
    (hash >> 16) as usize & (splits - 1)
}

/// The most splits an encoded count partition is cut into, which is what [`encoded_split`] has
/// bits for.
const ENCODED_SPLITS: usize = 256;

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
    // Split and folded a cache sized piece at a time, for the reason [`FIXED_SPLIT_ROWS`] gives.
    // On ClickBench 19 a partition is about a hundred thousand groups and the probe into its table
    // was a sixth of the query.
    let reserving = stage::Timing::start(Stage::Reserve);
    let total: usize = runs.runs.iter().map(EncodedCountRun::len).sum();
    let splits = (total / FIXED_SPLIT_ROWS).max(1).next_power_of_two().min(ENCODED_SPLITS);
    let share = total.div_ceil(splits);
    let share = (share + share.isqrt() * 4).min(total);
    let capacity = share.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width_of(
        capacity * size_of::<u32>()
            + share * size_of::<i64>()
            + total * size_of::<EncodedCountRecord>(),
    ))?;
    let mut parts: Vec<EncodedCountPartition> = (0..splits)
        .map(|_| EncodedCountPartition {
            rows: Vec::with_capacity(share),
            validity: Vec::new(),
            weights: Vec::new(),
        })
        .collect();
    reserving.stop(0);
    let timing = stage::Timing::start(Stage::Merge);
    for run in std::mem::take(&mut runs.runs) {
        let all_valid = run.group_validity.is_empty();
        // The weights are kept in step with the groups, so they sit in blocks of the same sizes and
        // are read a block alongside a block.
        let mut source = 0;
        for (block, weights) in run.groups.slices().zip(run.weights.slices()) {
            for (&row, &weight) in block.iter().zip(weights) {
                let valid =
                    if all_valid { EncodedCountRecord::ALL } else { run.group_validity[source] };
                parts[encoded_split(row.hash, splits)].push_weighted(row, valid, weight);
                source += 1;
            }
        }
        let all_valid = run.pending_validity.is_empty();
        let mut source = 0;
        for block in run.pending.slices() {
            for &row in block {
                let valid =
                    if all_valid { EncodedCountRecord::ALL } else { run.pending_validity[source] };
                parts[encoded_split(row.hash, splits)].push_weighted(row, valid, 1);
                source += 1;
            }
        }
    }
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Fold);
    let mut buckets: Vec<u32> = Vec::with_capacity(capacity);
    let mut counts: Vec<i64> = Vec::with_capacity(share);
    let mut output: Vec<(i64, Vec<Value>)> = Vec::new();
    for mut partition in parts {
        let rows = partition.rows.len();
        buckets.clear();
        buckets.resize(rows.saturating_mul(2).max(64).next_power_of_two(), EMPTY_SLOT);
        counts.clear();
        // The groups are compacted into the front of the split's own records: the group for a
        // record always lands at a slot at or behind where the record was read from.
        let all_valid = partition.validity.is_empty();
        for source in 0..rows {
            let row = partition.rows[source];
            let valid =
                if all_valid { EncodedCountRecord::ALL } else { partition.validity[source] };
            let weight = partition.weight(source);
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
                .checked_add(weight)
                .ok_or_else(|| Error::out_of_range("a grouped COUNT overflowed BIGINT"))?;
        }
        let best = largest(counts.len(), bound, |slot| counts[slot]);
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
            output.push((counts[slot], row));
        }
    }
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Emit);
    output.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
    output.truncate(bound);
    let output = output.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
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

/// What a distinct table of this many slots costs, the values and the bit a slot beside them.
fn distinct_table_bytes(capacity: usize) -> usize {
    capacity * size_of::<i64>() + capacity.div_ceil(8)
}

/// Doubles a distinct table and puts everything in it back.
///
/// Nothing but the value is stored, so the new place is worked out from the value the same way the
/// first one was. There is no hash to carry and nothing to compare on the way in, because a table
/// that is being rebuilt out of a table already holds each value once.
fn regrow_distinct(slots: &mut Vec<i64>, filled: &mut Vec<u64>) {
    let capacity = slots.len() * 2;
    let mask = capacity - 1;
    let mut next = vec![0_i64; capacity];
    let mut taken = vec![0_u64; capacity.div_ceil(64)];
    for (from, &value) in slots.iter().enumerate() {
        if filled[from / 64] & (1_u64 << (from % 64)) == 0 {
            continue;
        }
        let mut at = spread(mix(0, value as u64)) as usize & mask;
        while taken[at / 64] & (1_u64 << (at % 64)) != 0 {
            at = (at + 1) & mask;
        }
        taken[at / 64] |= 1_u64 << (at % 64);
        next[at] = value;
    }
    *slots = next;
    *filled = taken;
}

/// How many distinct values one radix partition holds, across the runs its instances handed over.
///
/// The table is the values themselves with a bit a slot saying which ones are filled, rather than an
/// index into the run the way it was when there was one run to index. Nothing is moved into place, so
/// the runs are only ever read.
///
/// It is sized by the distinct values it ends up holding rather than by the values that arrive,
/// because those are not the same number and on ClickBench they are not close. `COUNT(DISTINCT
/// UserID)` over `hits` scatters a million and a half values into a partition and keeps two hundred
/// and seventy thousand of them, so a table sized by what arrives is twelve times larger than the
/// one that is wanted, and with every partition being built at once that is a gigabyte of tables
/// that no probe ever hits twice. Doubling from small costs one rehash of what is in the table at
/// the time, which summed over every doubling is under twice the final contents, and buys a million
/// and a half probes into something that has a chance of being in cache.
fn bigint_distinct_partition(partition: &mut BigIntDistinctRuns, memory: &Memory) -> Result<i64> {
    let held: usize = partition.runs.iter().map(Vec::len).sum();
    let ceiling = held.saturating_mul(2).max(64).next_power_of_two();
    let mut capacity = ceiling.min(1024);
    let mut working = memory.reservation();
    working.grow(width_of(distinct_table_bytes(capacity)))?;
    let mut slots = vec![0_i64; capacity];
    let mut filled = vec![0_u64; capacity.div_ceil(64)];
    let mut mask = capacity - 1;
    let mut limit = capacity / 2;
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
            if unique >= limit && capacity < ceiling {
                working.grow(width_of(distinct_table_bytes(capacity)))?;
                regrow_distinct(&mut slots, &mut filled);
                capacity *= 2;
                mask = capacity - 1;
                limit = capacity / 2;
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

/// How many records one split of a fixed radix partition is sized to hold.
///
/// A radix partition of ClickBench 33 is 156 thousand groups, one per row, and its table is two
/// megabytes of buckets over two and a half of records, so on eight threads at once nearly every
/// probe was a miss out of the last level cache and the probe was half of the query. Splitting the
/// partition again by more bits of the hash, and folding one split at a time, leaves a table of a
/// hundred and twenty eight kilobytes over two hundred and fifty six of records, which stays in the
/// core's own cache for the whole fold. The split is one more pass over sixteen byte records, read
/// and written in order.
const FIXED_SPLIT_ROWS: usize = 16_384;

/// Which split of `splits` a fixed record belongs to, by bits of its hash that neither the radix
/// partition, the bucket tag nor a table this small reads.
#[inline]
fn fixed_split(hash: u64, splits: usize) -> usize {
    (hash >> 40) as usize & (splits - 1)
}

fn fixed_partition(
    runs: &mut FixedRuns,
    keys: &[LogicalType; 2],
    bound: usize,
    calls: &[Call],
    memory: &Memory,
) -> Result<Part> {
    let reserving = stage::Timing::start(Stage::Reserve);
    let total: usize = runs.runs.iter().map(|run| run.rows.len()).sum();
    let splits = (total / FIXED_SPLIT_ROWS).max(1).next_power_of_two();
    let share = total.div_ceil(splits);
    let share = (share + share.isqrt() * 4).min(total);
    let capacity = share.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width_of(
        capacity * size_of::<u32>()
            + share * size_of::<CompactNumeric>()
            + total * size_of::<FixedRecord>(),
    ))?;
    let mut parts: Vec<FixedPartition> = (0..splits)
        .map(|_| FixedPartition { rows: Vec::with_capacity(share), validity: Vec::new() })
        .collect();
    reserving.stop(0);
    let timing = stage::Timing::start(Stage::Merge);
    for run in std::mem::take(&mut runs.runs) {
        let all_valid = run.validity.is_empty();
        let mut source = 0;
        for block in run.rows.slices() {
            for &row in block {
                let valid = if all_valid { FixedRecord::ALL } else { run.validity[source] };
                parts[fixed_split(fixed_hash(row, valid), splits)].push(row, valid);
                source += 1;
            }
        }
    }
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Fold);
    let mut buckets: Vec<u32> = Vec::with_capacity(capacity);
    let mut states: Vec<CompactNumeric> = Vec::with_capacity(share);
    let mut output: Vec<(i64, Vec<Value>)> = Vec::new();
    for mut partition in parts {
        let rows = partition.rows.len();
        let capacity = rows.saturating_mul(2).max(64).next_power_of_two();
        buckets.clear();
        buckets.resize(capacity, EMPTY_SLOT);
        states.clear();
        let mut overflow = HashMap::new();
        let all_valid = partition.validity.is_empty();
        // The groups are compacted into the front of the split's own records, which is safe
        // because a new group's slot is never past the record that opened it.
        for source in 0..rows {
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
        let best = largest(states.len(), bound, |slot| states[slot].count());
        for slot in best {
            let key = partition.rows[slot];
            let valid = if all_valid { FixedRecord::ALL } else { partition.validity[slot] };
            let state = &states[slot];
            let (sum, mean) = state.totals(slot, &overflow);
            output.push((
                state.count(),
                vec![
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
                ],
            ));
        }
    }
    timing.stop(0);
    let timing = stage::Timing::start(Stage::Emit);
    output.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
    output.truncate(bound);
    let output = output.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
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
    bound: Option<usize>,
    constants: &[Option<Value>],
    group_types: &[LogicalType],
) -> Result<Vec<Chunk>> {
    let width = dictionary.len().saturating_add(DENSE_PARTITIONS - 1 - number) / DENSE_PARTITIONS;
    let rows: usize = partition.runs.iter().map(Blocks::len).sum();
    // The groups in code order either way, as a count per code of the partition's share of the
    // dictionary or, when far fewer rows arrived than there are codes, as the rows sorted and
    // counted in runs. ClickBench 38 groups by `Title`, whose dictionary is millions of codes, and a
    // filter leaves it a few thousand rows, so the array was megabytes of fresh pages to fault in
    // and zero and then read back to find those rows in.
    let mut groups: Vec<(u32, i64)> = if rows.saturating_mul(SPARSE_DENSE) < width {
        let mut sorted: Vec<u32> =
            // flatten: the codes arrive as blocks of slices, and sorting them needs one buffer.
            partition.runs.iter().flat_map(Blocks::slices).flatten().copied().collect();
        sorted.sort_unstable();
        sorted.chunk_by(|left, right| left == right).map(|run| (run[0], run.len() as i64)).collect()
    } else if u32::try_from(rows).is_ok() {
        dense_counts::<u32>(&partition.runs, width, number, bound)
    } else {
        dense_counts::<i64>(&partition.runs, width, number, bound)
    };
    // Under a TopN on the count only the `bound` largest groups of the partition can reach it, and
    // building the rest into chunks is most of the finish: ClickBench 34 groups ten million rows of
    // `URL` into millions of groups for a `LIMIT 10`. They stay in code order, which is the order
    // the TopN would have seen them in and settles its ties by.
    if let Some(bound) = bound.filter(|&bound| bound < groups.len()) {
        let mut best = largest(groups.len(), bound, |slot| groups[slot].1);
        best.sort_unstable();
        groups = best.into_iter().map(|slot| groups[slot]).collect();
    }
    let mut chunks = Vec::new();
    let mut codes = Vec::with_capacity(VECTOR_SIZE);
    let mut counts = Vec::with_capacity(VECTOR_SIZE);
    let mut valid = Vec::with_capacity(VECTOR_SIZE);
    for (code, count) in groups {
        codes.push(code);
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

/// How many rows of `runs` hold each code of partition `number`'s share of the dictionary, as the
/// codes seen and their counts in code order.
///
/// The counts are four bytes wide when the rows are few enough that no count can pass that, which
/// is every partition of a file under four billion rows, and it halves the array: `URL` on
/// ClickBench is seven million codes, so it is 14 MB against 28 for each of the four partitions
/// finishing at once.
///
/// Under a `bound` only the largest counts come back, picked out of the array where it lies. Every
/// group was listed first and cut down after, and on ClickBench 34 that list was a million sixteen
/// byte pairs a partition grown by doubling, a third of the page faults of the query for ten rows.
fn dense_counts<C>(
    runs: &[Blocks<u32>],
    width: usize,
    number: usize,
    bound: Option<usize>,
) -> Vec<(u32, i64)>
where
    C: Copy + Default + PartialEq + std::ops::AddAssign + From<u8> + Into<i64>,
{
    let mut dense = vec![C::default(); width];
    for run in runs {
        for block in run.slices() {
            for &code in block {
                dense[code as usize / DENSE_PARTITIONS] += C::from(1);
            }
        }
    }
    let code = |slot: usize| (slot * DENSE_PARTITIONS + number) as u32;
    if let Some(bound) = bound {
        // Largest first and in slot order among equals, which is code order, and a zero only
        // when fewer groups than the bound were seen, so those are dropped after.
        let mut best = largest(width, bound, |slot| dense[slot].into());
        best.retain(|&slot| dense[slot] != C::default());
        best.sort_unstable();
        return best.into_iter().map(|slot| (code(slot), dense[slot].into())).collect();
    }
    dense
        .iter()
        .enumerate()
        .filter(|(_, count)| **count != C::default())
        .map(|(slot, &count)| (code(slot), count.into()))
        .collect()
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
/// The rule itself is [`pairs::finish_degree`], which the grouped distinct finish shares, because
/// the two passes are the same shape: a thread takes a partition, probes a table and writes what it
/// finds. What is local to here is the two bounds it is capped by. There is no point starting more
/// threads than there are partitions to give them, which is what [`RADIX_PARTITIONS`] says, and
/// there is no point asking for more than the query was given, which is what the lease says and
/// what this used to ignore: a session that set the thread count to one still finished an aggregate
/// on sixteen.
fn degree_for(input: usize, threads: &Lease<'_>) -> usize {
    pairs::finish_degree(input, RADIX_PARTITIONS.min(threads.degree()))
}

/// How many rows of a fixed key partition are worth a thread while the ramp is still climbing.
///
/// See [`fixed_degree`].
const FIXED_ROWS_PER_THREAD: usize = 8_192;

/// How far that ramp climbs before the slower rule takes over. See [`fixed_degree`].
const FIXED_RAMP: usize = 16;

/// How many threads to finish `input` rows of fixed key partitions on.
///
/// The same two rules as [`pairs::finish_degree`] with twice the ramp and twice the ceiling on it,
/// because this finish is not waiting on the same thing the others are. A distinct finish probes a
/// table with a slot per distinct pair and spends its time waiting on memory, so a thread past the
/// machine's memory level parallelism buys nothing. This one folds a partition's records into its
/// groups and runs the aggregate calls over them, which is arithmetic, and arithmetic keeps scaling
/// for longer.
///
/// Measured on the million row ClickBench file, the two queries that land here finish a hundred and
/// thirty thousand records. Swept by hand, two threads take 2.718 ms, eight take 1.661, twelve take
/// 1.593, sixteen take 1.613 and thirty two take 1.722. So the useful window is twelve to sixteen
/// where the shared rule asks for eight, and it still turns over well before the whole machine.
fn fixed_degree(input: usize, threads: &Lease<'_>) -> usize {
    let quickly = input.div_ceil(FIXED_ROWS_PER_THREAD).min(FIXED_RAMP);
    let slowly = input.div_ceil(pairs::ROWS_PER_EXTRA_THREAD);
    quickly.max(slowly).clamp(1, RADIX_PARTITIONS.min(threads.degree()))
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

/// Whether a group key is stored in a fixed number of bytes, so a group of them carries nothing
/// beside the table. See [`FIXED_PARTITION_FROM`].
fn fixed_width(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
            | LogicalType::Float
            | LogicalType::Double
            | LogicalType::Decimal { .. }
            | LogicalType::Uuid
            | LogicalType::Date
            | LogicalType::Time
            | LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs
            | LogicalType::TimestampTz
    )
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
    use rudb_kernels::NOWHERE;
    use rudb_pipeline::Sink;
    use rudb_plan::{Plan, Slice};
    use rudb_vector::{Chunk, Data, Vector};

    use super::{
        Aggregate, BigIntDistinct, BigIntDistinctRuns, COMPACT_FROM, Call, CompactNumeric,
        Distinct, EncodedCountRecord, EncodedCountRun, EncodedCountRuns, FixedPartition,
        FixedRecord, FixedRun, FixedRuns, PARTITION_FROM, RADIX_PARTITIONS, RUN_BLOCK, Share,
        Signed, WINDOW_RATE, WINDOW_SLACK, bigint_distinct_partition, encoded_count_partition,
        fixed_partition, slot_runs_of, spread_runs, spread_slots,
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

    /// Runs of kept rows moved onto all the rows come out as the runs that spreading the slots and
    /// cutting them again finds, with dropped rows before, inside, between and after the runs.
    #[test]
    fn runs_of_kept_rows_spread_like_their_slots() {
        let all = 200;
        let kept: Vec<u32> =
            (0..all as u32).filter(|row| !matches!(row % 37, 0 | 5 | 6) && *row < 190).collect();
        let slots: Vec<usize> = (0..kept.len()).map(|at| [3, 3, 9, 1][at / 30 % 4]).collect();
        let mut runs = Vec::new();
        assert!(slot_runs_of(&slots, &mut runs, 1));
        let mut spread = Vec::new();
        assert!(spread_runs(&runs, &kept, all, usize::MAX, &mut spread));
        let mut moved = slots.clone();
        spread_slots(&mut moved, &kept, all);
        let mut expected = Vec::new();
        assert!(slot_runs_of(&moved, &mut expected, usize::MAX));
        assert_eq!(spread, expected);
        assert!(!spread_runs(&runs, &kept, all, 3, &mut spread));
        assert!(spread.is_empty());
        let whole: Vec<u32> = (0..all as u32).collect();
        assert!(spread_runs(&[(5, 150), (2, 200)], &whole, all, 2, &mut spread));
        assert_eq!(spread, [(5, 150), (2, 200)]);
    }

    /// Slots cut into runs come back as the runs they are, across the blocks the cut reads them in,
    /// and slots in no order come back as nothing.
    #[test]
    fn slots_in_runs_are_cut_into_them_and_slots_in_no_order_are_not() {
        let lengths = [(4, 40), (NOWHERE, 17), (1, 1), (4, 30), (0, 16)];
        let slots: Vec<usize> =
            lengths.iter().flat_map(|&(slot, length)| std::iter::repeat_n(slot, length)).collect();
        let mut runs = Vec::new();
        assert!(slot_runs_of(&slots, &mut runs, 1));
        let mut end = 0;
        let expected: Vec<(usize, usize)> = lengths
            .iter()
            .map(|&(slot, length)| {
                end += length;
                (slot, end)
            })
            .collect();
        assert_eq!(runs, expected);
        let one = vec![7; 1_000];
        assert!(slot_runs_of(&one, &mut runs, 1));
        assert_eq!(runs, [(7, 1_000)]);
        let scattered: Vec<usize> = (0..1_000).map(|row| row * 7 % 13).collect();
        assert!(!slot_runs_of(&scattered, &mut runs, 1));
        assert!(runs.is_empty());
        assert!(!slot_runs_of(&[], &mut runs, 1));
    }

    /// The budget is per call that will read the runs, so slots one call gives up on are cut for
    /// eight, which is the shape of q01: runs of one or two rows that eight aggregates all read.
    #[test]
    fn slots_too_broken_up_for_one_call_are_cut_for_eight_of_them() {
        let scattered: Vec<usize> = (0..1_000).map(|row| row * 7 % 13).collect();
        let mut runs = Vec::new();
        assert!(!slot_runs_of(&scattered, &mut runs, 1));
        assert!(slot_runs_of(&scattered, &mut runs, 8));
        assert_eq!(runs.len(), 1_000);
        assert_eq!(runs.last(), Some(&(scattered[999], 1_000)));
        // No call reading them is a chunk nobody would pay the pass for.
        assert!(!slot_runs_of(&scattered, &mut runs, 0));
        assert!(runs.is_empty());
    }

    /// A run that ends exactly where a block of the mask does, and one that ends one row either side
    /// of it, since the mask reads a row against the row before it and those three are where the bit
    /// for a start and the slot the run it ends carries come from different blocks.
    #[test]
    fn runs_ending_on_the_edge_of_a_block_are_cut_where_they_end() {
        for first in [RUN_BLOCK - 1, RUN_BLOCK, RUN_BLOCK + 1] {
            let lengths = [(3, first), (8, 1), (3, RUN_BLOCK * 2), (5, 2)];
            let slots: Vec<usize> = lengths
                .iter()
                .flat_map(|&(slot, length)| std::iter::repeat_n(slot, length))
                .collect();
            let mut runs = Vec::new();
            assert!(slot_runs_of(&slots, &mut runs, 8), "gave up on runs ending at {first}");
            let mut end = 0;
            let expected: Vec<(usize, usize)> = lengths
                .iter()
                .map(|&(slot, length)| {
                    end += length;
                    (slot, end)
                })
                .collect();
            assert_eq!(runs, expected, "the runs ending at {first} were cut wrong");
        }
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
    /// Twenty thousand groups is past [`FIXED_PARTITION_FROM`], so the instances hand their tables
    /// to the partitions and `finalize` finishes those partitions in parallel. What this pins is
    /// that every group comes out exactly once. A partition finished twice doubles its counts and
    /// one nobody finished loses its groups, and neither can happen on a table small enough to stay
    /// in one piece, which is every other test in here.
    #[test]
    fn a_partitioned_aggregate_answers_every_group_once() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..20_000).collect();
        for part in values.chunks(1_024) {
            aggregate.sink(&chunk(part), &mut left).expect("a chunk of groups");
            aggregate.sink(&chunk(part), &mut right).expect("the same groups again");
        }
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        let built = aggregate.built.lock().expect("readable");
        assert!(
            built.partitioning,
            "twenty thousand groups on two instances is meant to take the partitioned path"
        );
        assert!(built.local, "twenty thousand groups fit in the cache twice over");
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

    /// The same five thousand groups, under a pushed down bound, are split all the same.
    ///
    /// The bound is applied when a table is finished, so a split applies it once per partition and
    /// hands the pipeline above more rows than one table would have. That used to be reason enough
    /// to keep the table whole, and the threshold which did it stopped ClickBench 40 splitting at
    /// any thread count and cost it 1.62 times. The rows a split hands up are cheap since #1108,
    /// #1111 and #1123, and the aggregate scaling is worth more than the reduction, so only the
    /// group count has a say now.
    ///
    /// Twenty thousand groups is over [`FIXED_PARTITION_FROM`] so this splits, and a bound of a
    /// thousand is over the three hundred groups a partition is left holding, so no partition
    /// throws anything away and the pipeline above gets all twenty thousand. The TopN up there is
    /// what makes that right: reducing here is an optimisation and never the thing that gives the
    /// answer.
    #[test]
    fn an_aggregate_under_a_pushed_down_bound_is_split_all_the_same() {
        // Two calls, so that the counted exchange, which owns a lone count, leaves this to the
        // partitions this test is about.
        let plan = parsed(
            "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT, count_star()::BIGINT]",
        );
        let (aggregate, out) = aggregate(&plan);
        let aggregate = aggregate.top_counts(1_000, 0);
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..20_000).collect();
        for part in values.chunks(1_024) {
            aggregate.sink(&chunk(part), &mut left).expect("a chunk of groups");
            aggregate.sink(&chunk(part), &mut right).expect("the same groups again");
        }
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        assert!(
            aggregate.built.lock().expect("readable").partitioning,
            "twenty thousand groups is over the line whatever the bound above says"
        );
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(
            answer(&out).len(),
            20_000,
            "a bound over what a partition holds throws nothing away"
        );
    }

    /// A lone count over one integer key under a bound goes to the counted exchange, which has to add
    /// up what two instances saw of the same groups, keep the nulls as one group and hand back at
    /// least the largest groups.
    #[test]
    fn a_counted_exchange_adds_instances_and_keeps_the_largest_groups() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, out) = aggregate(&plan);
        let aggregate = aggregate.top_counts(2, 0);
        assert!(aggregate.counted_top_count());
        let mut left = aggregate.local();
        let mut right = aggregate.local();
        let values: Vec<i32> = (0..50_000).collect();
        for part in values.chunks(1_024) {
            aggregate.sink(&chunk(part), &mut left).expect("a chunk of groups");
        }
        aggregate.sink(&chunk(&[7, 7, 7, 9, 7]), &mut right).expect("a run and a repeat");
        let nulls = Vector::from_values(LogicalType::Integer, &[Value::Null, Value::Integer(9)])
            .expect("INTEGER values");
        aggregate
            .sink(&Chunk::new(vec![nulls]).expect("one column"), &mut right)
            .expect("a null key");
        aggregate.combine(left).expect("the first instance");
        aggregate.combine(right).expect("the second instance");
        aggregate.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        let mut rows = answer(&out);
        rows.sort_by_key(|row| match row[1] {
            Value::BigInt(count) => std::cmp::Reverse(count),
            ref other => panic!("a count of {other:?}"),
        });
        assert_eq!(rows[0], vec![Value::Integer(7), Value::BigInt(5)]);
        assert_eq!(rows[1], vec![Value::Integer(9), Value::BigInt(3)]);
        assert!(rows.contains(&vec![Value::Null, Value::BigInt(1)]), "the null group is kept");
        assert!(rows.len() < 50_000, "each split keeps its two largest and no more");
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
        let mut partition = EncodedCountRun::default();
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
    fn an_encoded_count_partition_split_for_cache_adds_weights_across_runs_and_splits() {
        let dictionary = Vector::from_values(LogicalType::Varchar, &[Value::Varchar("one".into())])
            .expect("a string dictionary");
        // A hash that spreads the groups over every split, and a first run compacted so that its
        // records carry weights while the second's are single rows.
        let groups = (super::FIXED_SPLIT_ROWS * 8) as i64;
        let row = |first: i64| EncodedCountRecord {
            first,
            second: 0,
            hash: (first as u32).wrapping_mul(0x9e37_79b9),
            third: 0,
        };
        let mut early = EncodedCountRun::default();
        let mut late = EncodedCountRun::default();
        for first in 0..groups {
            let times = if first % 10_000 == 0 { 2 + first / 10_000 } else { 1 };
            for time in 0..times {
                let into = if time % 2 == 0 { &mut early } else { &mut late };
                into.push(row(first), EncodedCountRecord::ALL);
            }
        }
        early.compact();
        let part = encoded_count_partition(
            &mut EncodedCountRuns { runs: vec![early, late] },
            &dictionary,
            &[LogicalType::BigInt],
            3,
            &Memory::unlimited(),
        )
        .expect("the encoded partition");
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for chunk in part.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        let largest = groups / 10_000 * 10_000;
        let expected = (0..3)
            .map(|step| {
                let first = largest - step * 10_000;
                vec![
                    Value::BigInt(first),
                    Value::Varchar("one".into()),
                    Value::BigInt(2 + first / 10_000),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, expected, "the three largest groups, largest first");
    }

    #[test]
    fn an_encoded_count_partition_folds_every_run_into_the_widest_one() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        let row = |first, third| EncodedCountRecord { first, second: 0, hash: 7, third };
        let mut narrow = EncodedCountRun::default();
        narrow.push(row(1, 0), EncodedCountRecord::ALL);
        let mut widest = EncodedCountRun::default();
        for _ in 0..3 {
            widest.push(row(1, 0), EncodedCountRecord::ALL);
        }
        widest.push(row(2, 1), EncodedCountRecord::ALL);
        // The run that carries the only null is not the one the fold takes as its table, so the
        // table starts out with no validity at all and has to grow one when this arrives.
        let mut late = EncodedCountRun::default();
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
    fn a_full_encoded_run_folds_its_repeats_and_the_counts_come_out_the_same() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        // Five groups, one of them with a null key, sharing a hash so every probe walks past the
        // others, scattered round after round until the run has filled and folded several times.
        let row = |first, third| EncodedCountRecord { first, second: 0, hash: 7, third };
        let mut folded = EncodedCountRun::default();
        let rounds = COMPACT_FROM * 3;
        for round in 0..rounds {
            folded.scatter(row(round as i64 % 4, (round % 2) as u32), EncodedCountRecord::ALL);
            if round % 3 == 0 {
                folded.scatter(row(0, 1), EncodedCountRecord::SECOND | EncodedCountRecord::THIRD);
            }
        }
        assert!(
            folded.len() <= COMPACT_FROM * 2 && folded.room <= COMPACT_FROM * 2,
            "a run of five groups grew to {}",
            folded.len()
        );
        let mut other = EncodedCountRun::default();
        other.scatter(row(1, 1), EncodedCountRecord::ALL);
        let leading = [LogicalType::BigInt];
        let part = encoded_count_partition(
            &mut EncodedCountRuns { runs: vec![other, folded] },
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
        let quarter = (rounds / 4) as i64;
        let mut expected = vec![
            vec![Value::BigInt(0), Value::Varchar("one".into()), Value::BigInt(quarter)],
            vec![Value::BigInt(1), Value::Varchar("two".into()), Value::BigInt(quarter + 1)],
            vec![Value::BigInt(2), Value::Varchar("one".into()), Value::BigInt(quarter)],
            vec![Value::BigInt(3), Value::Varchar("two".into()), Value::BigInt(quarter)],
            vec![
                Value::Null,
                Value::Varchar("two".into()),
                Value::BigInt(rounds.div_ceil(3) as i64),
            ],
        ];
        expected.sort_by_key(|row| format!("{row:?}"));
        assert_eq!(rows, expected, "a folded run counts every row it was given");
    }

    #[test]
    fn a_two_key_encoded_count_omits_the_unused_integer_and_narrows_the_one_it_keeps() {
        let dictionary = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("one".into()), Value::Varchar("two".into())],
        )
        .expect("a string dictionary");
        let row = |first, third| EncodedCountRecord { first, second: 0, hash: 7, third };
        let mut partition = EncodedCountRun::default();
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
        let mut encoded = EncodedCountRun::default();
        let row = EncodedCountRecord { first: 0, second: 0, hash: 7, third: 0 };
        let some = EncodedCountRecord::SECOND | EncodedCountRecord::THIRD;
        encoded.push(row, some);
        encoded.push(row, EncodedCountRecord::ALL);
        assert_eq!(encoded.pending_validity, vec![some, EncodedCountRecord::ALL]);
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
        let mut partition = FixedRun::default();
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
                reads_total: None,
                repeats: None,
            },
            Call {
                name: "sum".into(),
                args: Vec::new(),
                distinct: false,
                filter: None,
                returns: LogicalType::HugeInt,
                affine: None,
                reads_total: None,
                repeats: None,
            },
            Call {
                name: "avg".into(),
                args: Vec::new(),
                distinct: false,
                filter: None,
                returns: LogicalType::Double,
                affine: None,
                reads_total: None,
                repeats: None,
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

    #[test]
    fn a_fixed_partition_split_for_cache_keeps_the_largest_groups_of_every_split() {
        // Enough groups for eight splits, each key seen once or more across two runs, so that the
        // largest groups land in different splits and a group's rows come from both runs.
        let groups = super::FIXED_SPLIT_ROWS as i64 * 8;
        let row = |first: i64| FixedRecord { first, second: (first % 7) as i32, sum: 1, mean: 2 };
        let (mut early, mut late) = (FixedRun::default(), FixedRun::default());
        for first in 0..groups {
            let times = if first % 5_000 == 0 { 3 + first / 5_000 } else { 1 };
            for time in 0..times {
                let into = if time % 2 == 0 { &mut early } else { &mut late };
                into.push(row(first), FixedRecord::ALL);
            }
        }
        let call = |name: &str, returns| Call {
            name: name.into(),
            args: Vec::new(),
            distinct: false,
            filter: None,
            returns,
            affine: None,
            reads_total: None,
            repeats: None,
        };
        let calls = [
            call("count_star", LogicalType::BigInt),
            call("sum", LogicalType::HugeInt),
            call("avg", LogicalType::Double),
        ];
        let part = fixed_partition(
            &mut FixedRuns { runs: vec![early, late] },
            &[LogicalType::BigInt, LogicalType::Integer],
            4,
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
        let largest = groups / 5_000 * 5_000;
        let expected = (0..4)
            .map(|step| {
                let first = largest - step * 5_000;
                let times = 3 + first / 5_000;
                vec![
                    Value::BigInt(first),
                    Value::Integer((first % 7) as i32),
                    Value::BigInt(times),
                    Value::HugeInt(i128::from(times)),
                    Value::Double(2.0),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, expected, "the four largest groups, largest first");
    }

    #[test]
    fn the_largest_counts_come_back_largest_first_and_in_slot_order_among_equals() {
        let counts = [3, 1, 5, 3, 5, 1, 2, 3];
        assert_eq!(super::largest(counts.len(), 4, |slot| counts[slot]), [2, 4, 0, 3]);
        assert_eq!(super::largest(counts.len(), 0, |slot| counts[slot]), [0_usize; 0]);
        assert_eq!(super::largest(counts.len(), 20, |slot| counts[slot]), [2, 4, 0, 3, 7, 6, 1, 5]);
        // Every slot against a bound of one, which only a strictly larger count displaces.
        assert_eq!(super::largest(counts.len(), 1, |slot| counts[slot]), [2]);
    }

    #[test]
    fn a_dense_partition_sorts_a_few_rows_into_the_answer_the_array_gives() {
        let spellings =
            (0..1_000).map(|code| Value::Varchar(format!("v{code}"))).collect::<Vec<_>>();
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &spellings).expect("a dictionary"));
        let bounded = |runs: Vec<Vec<u32>>, bound: Option<usize>| {
            let runs = runs.into_iter().map(|run| run.into_iter().collect()).collect();
            let mut partition = super::DensePartition { runs, nulls: 0 };
            let chunks = super::dense_partition(
                &dictionary,
                1,
                &mut partition,
                bound,
                &[None],
                &[LogicalType::Varchar],
            )
            .expect("the dense partition");
            let mut rows: Vec<(Value, Value)> = Vec::new();
            for chunk in chunks {
                for row in 0..chunk.len() {
                    rows.push((chunk.value_at(row, 0), chunk.value_at(row, 1)));
                }
            }
            rows
        };
        let answer = |runs: Vec<Vec<u32>>| bounded(runs, None);
        let codes = [405, 9, 5, 9, 405, 405, 997];
        // Seven rows against 250 codes is the sorted path, and the same codes seven hundred times
        // over is the array. Both answer in code order.
        let few = answer(vec![codes[..4].to_vec(), codes[4..].to_vec()]);
        let many = answer(vec![codes.repeat(100)]);
        let expected = |times: i64| {
            [(5, 1), (9, 2), (405, 3), (997, 1)]
                .map(|(code, count)| {
                    (Value::Varchar(format!("v{code}")), Value::BigInt(count * times))
                })
                .to_vec()
        };
        assert_eq!(few, expected(1));
        assert_eq!(many, expected(100));
        // A TopN bound keeps the largest groups of either path, still in code order.
        let top = |times: i64| {
            [(9, 2), (405, 3)]
                .map(|(code, count)| {
                    (Value::Varchar(format!("v{code}")), Value::BigInt(count * times))
                })
                .to_vec()
        };
        assert_eq!(bounded(vec![codes.to_vec()], Some(2)), top(1));
        assert_eq!(bounded(vec![codes.repeat(100)], Some(2)), top(100));
        assert_eq!(bounded(vec![codes.repeat(100)], Some(4)), expected(100));
    }

    #[test]
    fn the_one_table_an_aggregate_answers_out_of_takes_room_for_every_group() {
        assert_eq!(Share::Whole.of(8 << 20), Some(8 << 20));
        assert_eq!(Share::Whole.of(1), Some(1));
    }

    #[test]
    fn a_partition_takes_room_for_its_share() {
        let groups = 8 << 20;
        assert_eq!(Share::Partition.of(groups), Some(groups / RADIX_PARTITIONS as u64));
        assert_eq!(Share::Partition.of(6400), Some(100));
    }

    #[test]
    fn a_partition_of_a_small_aggregate_still_gets_a_group() {
        for groups in 0..RADIX_PARTITIONS as u64 {
            assert_eq!(Share::Partition.of(groups), Some(1), "{groups} groups");
        }
    }

    /// The two tables that never hold what the number counts, which is where the number was costing
    /// half a gigabyte of pages a query on q18.
    #[test]
    fn the_tables_that_pass_the_groups_on_take_no_more_room_than_they_will_use() {
        // Given up at `PARTITION_FROM` groups however many the aggregate ends with, so that is all
        // the room worth taking, and a smaller aggregate still asks for only what it will hold.
        assert_eq!(Share::Passing.of(8 << 20), Some(PARTITION_FROM as u64));
        assert_eq!(Share::Passing.of(100), Some(100));
        // One of these per partition per instance rather than per aggregate, so it starts where
        // every table started before any of this existed.
        assert_eq!(Share::Local.of(8 << 20), None);
        assert_eq!(Share::Local.of(1), None);
    }

    /// Folds `chunks` into one table the way a single instance does and hands it back.
    fn folded(aggregate: &Aggregate<'_>, chunks: &[Vec<i32>]) -> super::Building {
        let mut local = aggregate.local();
        let mut building = aggregate.starting(Share::Local);
        for part in chunks {
            let rows = aggregate.read(&chunk(part), &mut local.expressions).expect("a chunk");
            aggregate.fold(&rows, &mut building, None, None).expect("folded");
        }
        building
    }

    /// A sorted key moves past any window it is given, so every chunk would build a new map. The
    /// maps it builds are held to [`WINDOW_RATE`] places a row folded, which is what kept q18's
    /// `GROUP BY l_orderkey` from clearing a quarter of a million places per chunk per partition.
    #[test]
    fn a_key_that_keeps_moving_past_its_window_builds_maps_only_as_the_rows_pay_for_them() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, _out) = aggregate(&plan);
        let chunks: Vec<Vec<i32>> =
            (0..100).map(|at| (at * 2_048..(at + 1) * 2_048).collect()).collect();
        let building = folded(&aggregate, &chunks);
        assert_eq!(building.coded_read, 100 * 2_048);
        assert!(building.coded_spent > 0, "the first chunks are read by value");
        assert!(
            building.coded_spent <= building.coded_read * WINDOW_RATE + WINDOW_SLACK,
            "{} places cleared for {} rows",
            building.coded_spent,
            building.coded_read
        );
    }

    /// The other side of it, a key the way `CounterID` is, a few thousand values that come back in
    /// every chunk. Its window settles and the map is still the one reading it at the end.
    #[test]
    fn a_key_that_stays_inside_its_window_keeps_its_map() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, _out) = aggregate(&plan);
        let chunks: Vec<Vec<i32>> = (0..100)
            .map(|at| (0..2_048).map(|row| (row * 7 + at * 13) % 3_000 + 17).collect())
            .collect();
        let building = folded(&aggregate, &chunks);
        assert!(!building.coded_on.is_empty(), "the last chunk was answered by the map");
        assert!(building.coded_spent < 64 * 1_024, "the window settled after a few builds");
    }

    /// A key that climbs the way a sorted one does keeps the bottom of its window, so its map grows
    /// upward where it is and no place in it is paid for twice.
    #[test]
    fn a_key_that_climbs_grows_its_map_in_place() {
        let plan = parsed("Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]");
        let (aggregate, _out) = aggregate(&plan);
        let chunks: Vec<Vec<i32>> =
            (0..32).map(|at| (at * 1_024..(at + 1) * 1_024).collect()).collect();
        let building = folded(&aggregate, &chunks);
        assert!(!building.coded_on.is_empty(), "the last chunk was answered by the map");
        assert!(building.coded_map.len() > 32 * 1_024, "the map covers every value");
        assert_eq!(building.coded_spent, building.coded_map.len(), "no place was cleared twice");
    }

    /// Only the table an instance fills before it partitions covers the aggregate's whole key range,
    /// so only that one can carry a direct index over it.
    #[test]
    fn a_direct_index_over_the_range_goes_on_the_table_that_sees_the_whole_range() {
        assert!(Share::Whole.before_the_split());
        assert!(Share::Passing.before_the_split());
        assert!(!Share::Partition.before_the_split());
        assert!(!Share::Local.before_the_split());
    }
}
