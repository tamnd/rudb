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
    Error, Field, LogicalType, Memory, Reservation, Result, Session, Spent, Stage, Value, stage,
};
use rudb_kernels::{Accumulator, NOWHERE, is_true, update_scattered};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::buffer::Buffered;
use crate::key::{BigIntSet, Key, RowSet, mix, spread};
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
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
/// A grouped aggregate hashes a chunk once and divides its rows by the high four hash bits. Each of
/// the sixteen partitions owns one table behind its own lock. Workers can update different tables
/// together, while equal keys always reach the same table and are stored once. This avoids both the
/// duplicate table memory and the second probe that a merge of per-worker tables requires.
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
    /// An ungrouped COUNT(DISTINCT BIGINT) can exchange integer rows directly and count one set per
    /// radix owner instead of building and merging one general aggregate state per worker.
    radix_distinct_count: bool,
    /// Emit at most this many groups from each radix partition when the parent orders by count
    /// descending. The ordinary TopN still makes the final global choice.
    top_counts: Option<usize>,
    /// Emit only groups whose COUNT(*) call at this index reaches the inclusive bound.
    ///
    /// The Filter remains above the aggregate and checks the predicate again. This only avoids
    /// materializing groups that cannot pass it, so a missed shape is slow and never changes an
    /// answer.
    having_count: Option<(usize, i64)>,
    /// The most groups an unordered limit above this operator can observe.
    max_groups: Option<usize>,
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
    partitions: Vec<Mutex<FixedPartition>>,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug)]
struct BigIntDistinctExchange {
    partitions: Vec<Mutex<BigIntDistinctPartition>>,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug, Clone, Copy)]
struct BigIntDistinctRecord {
    hash: u64,
    value: i64,
}

#[derive(Debug, Default)]
struct BigIntDistinctPartition {
    rows: Vec<BigIntDistinctRecord>,
}

impl BigIntDistinctPartition {
    fn append(&mut self, other: &mut Self) {
        if self.rows.is_empty() {
            std::mem::swap(self, other);
        } else {
            self.rows.append(&mut other.rows);
        }
    }

    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<BigIntDistinctRecord>()
    }
}

#[derive(Debug, Clone, Copy)]
struct FixedRecord {
    hash: u64,
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

impl FixedPartition {
    fn push(&mut self, row: FixedRecord, valid: u8) {
        if valid != FixedRecord::ALL && self.validity.is_empty() {
            self.validity.resize(self.rows.len(), FixedRecord::ALL);
        }
        self.rows.push(row);
        if !self.validity.is_empty() {
            self.validity.push(valid);
        }
    }

    fn append(&mut self, other: &mut Self) {
        if self.rows.is_empty() {
            std::mem::swap(self, other);
            return;
        }
        if self.validity.is_empty() && !other.validity.is_empty() {
            self.validity.resize(self.rows.len(), FixedRecord::ALL);
        }
        let incoming = other.rows.len();
        self.rows.append(&mut other.rows);
        if self.validity.is_empty() {
            debug_assert!(other.validity.is_empty());
        } else if other.validity.is_empty() {
            self.validity.resize(self.validity.len() + incoming, FixedRecord::ALL);
        } else {
            self.validity.append(&mut other.validity);
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
    /// threads and a reservation belongs to the thread growing it. Moving sixteen charges into one
    /// at the end would mean holding both the old and the new charge for as long as the move took,
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
    /// thread closes this partition, which is one thread per partition and so sixteen merges
    /// running at once rather than one.
    pending: Vec<Building>,
}

const RADIX_PARTITIONS: usize = 16;
const DENSE_PARTITIONS: usize = 4;

/// How many groups an instance holds before it stops keeping them to itself.
///
/// Partitioning is not free. Every chunk is hashed, split, and gathered into one set of vectors per
/// partition, which is a copy of every column it carries, and then sixteen locks are taken to fold
/// the pieces. On a small aggregate that is all cost: ClickBench at a thousand rows ran 41 percent
/// slower and at ten thousand rows 19 percent slower when every grouped aggregate partitioned from
/// its first chunk, because none of those tables is large enough for the sharing to pay for itself.
///
/// Four thousand is where the measurement put it. Sixteen thousand was tried first, on the argument
/// that it is where a table stops fitting comfortably in cache, and it left a five percent loss at a
/// million rows: an instance that holds sixteen thousand groups to itself is an instance the other
/// threads cannot help with. At four thousand, ClickBench on gamingpc-wsl runs 2.8 times faster at a
/// thousand rows, 1.8 times at ten thousand and 1.44 times at a million, all against main, on the
/// same peak memory. Both numbers were measured in the same sweep and the lower one won everywhere.
const PARTITION_FROM: usize = 4_096;

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
            radix_distinct_count,
            top_counts: None,
            having_count: None,
            max_groups: None,
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
    #[must_use]
    pub(crate) fn top_counts(mut self, bound: usize) -> Self {
        if self.count_only || self.compact_numeric || self.distinct_count {
            self.top_counts = Some(bound);
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

    fn fixed_top_count(&self) -> bool {
        self.compact_numeric
            && self.top_counts.is_some()
            && self.constants.iter().all(Option::is_none)
            && self.keys.len() == 2
            && self.plan.expr_type(self.keys[0]) == &LogicalType::BigInt
            && self.plan.expr_type(self.keys[1]) == &LogicalType::Integer
    }

    fn buffer_fixed(
        &self,
        rows: &Rows,
        partitions: &mut [FixedPartition],
        memory: &mut Reservation,
    ) -> Result<()> {
        self.fixed.get_or_init(|| FixedExchange {
            partitions: (0..RADIX_PARTITIONS)
                .map(|_| Mutex::new(FixedPartition::default()))
                .collect(),
            held: Mutex::new(Vec::new()),
        });
        let [first, second] = rows.keys.as_slice() else {
            return Err(Error::internal("a fixed radix exchange received the wrong key width"));
        };
        let sum = rows.arguments[1].first().expect("SUM has one argument");
        let mean = rows.arguments[2].first().expect("AVG has one argument");
        let before = partitions.iter().map(FixedPartition::footprint).sum::<usize>();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        for row in 0..rows.rows {
            let mut valid = 0;
            let first_value = if first.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::FIRST;
                i64::try_from(first.signed_at(row).ok_or_else(|| {
                    Error::internal("a fixed BIGINT key has no signed representation")
                })?)
                .map_err(|_| Error::internal("a fixed BIGINT key is out of range"))?
            };
            let second_value = if second.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::SECOND;
                i32::try_from(second.signed_at(row).ok_or_else(|| {
                    Error::internal("a fixed INTEGER key has no signed representation")
                })?)
                .map_err(|_| Error::internal("a fixed INTEGER key is out of range"))?
            };
            let sum_value = if sum.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::SUM;
                i16::try_from(sum.signed_at(row).ok_or_else(|| {
                    Error::internal("a fixed SMALLINT sum has no signed representation")
                })?)
                .map_err(|_| Error::internal("a fixed SMALLINT sum is out of range"))?
            };
            let mean_value = if mean.is_null_at(row) {
                0
            } else {
                valid |= FixedRecord::MEAN;
                i16::try_from(mean.signed_at(row).ok_or_else(|| {
                    Error::internal("a fixed SMALLINT mean has no signed representation")
                })?)
                .map_err(|_| Error::internal("a fixed SMALLINT mean is out of range"))?
            };
            const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;
            let first_word =
                if valid & FixedRecord::FIRST != 0 { first_value as u64 } else { NOTHING };
            let second_word = if valid & FixedRecord::SECOND != 0 {
                i64::from(second_value) as u64
            } else {
                NOTHING
            };
            let hash = spread(mix(mix(0, first_word), second_word));
            partitions[(hash >> shift) as usize].push(
                FixedRecord {
                    hash,
                    first: first_value,
                    second: second_value,
                    sum: sum_value,
                    mean: mean_value,
                },
                valid,
            );
        }
        let after = partitions.iter().map(FixedPartition::footprint).sum::<usize>();
        memory.grow(width_of(after.saturating_sub(before)))
    }

    fn buffer_bigint_distinct(
        &self,
        rows: &Rows,
        partitions: &mut [BigIntDistinctPartition],
        memory: &mut Reservation,
    ) -> Result<()> {
        self.bigint_distinct.get_or_init(|| BigIntDistinctExchange {
            partitions: (0..RADIX_PARTITIONS)
                .map(|_| Mutex::new(BigIntDistinctPartition::default()))
                .collect(),
            held: Mutex::new(Vec::new()),
        });
        let Some(column) = rows.arguments.first().and_then(|arguments| arguments.first()) else {
            return Err(Error::internal("a BIGINT distinct exchange received no argument"));
        };
        let before = partitions.iter().map(BigIntDistinctPartition::footprint).sum::<usize>();
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        for row in 0..rows.rows {
            if column.is_null_at(row) {
                continue;
            }
            let value = i64::try_from(column.signed_at(row).ok_or_else(|| {
                Error::internal("a distinct BIGINT value has no signed representation")
            })?)
            .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?;
            let hash = spread(mix(0, value as u64));
            partitions[(hash >> shift) as usize].rows.push(BigIntDistinctRecord { hash, value });
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
        if !alone {
            match prehashed {
                Some(prehashed) => {
                    hashes.clear();
                    hashes.extend_from_slice(prehashed);
                }
                None => crate::table::hash(keys, *length, hashes),
            }
        }
        // The probe, and nothing else. What comes out of it is one slot per row, which is what the
        // scatter below needs and what the row loop used to consume as it went.
        slots.clear();
        slots.resize(*length, if alone { 0 } else { NOWHERE });
        // A batch at a time, because a probe of a table larger than the cache is three dependent
        // misses on a row and the only way to overlap them is to have several rows in flight at once.
        // What comes back is every row whose key is already a group, filled in, and the rest in row
        // order. Those go one at a time: a key that is not in the table either starts a group or goes
        // out to the spill file, and both of them change what the row after would have found.
        let mut from = 0;
        while !alone && from < *length {
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
            states,
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
            (Some(bound), _) => {
                let mut best = Vec::with_capacity(bound.min(groups));
                for slot in 0..groups {
                    let at = best.partition_point(|&kept| count(kept, 0) >= count(slot, 0));
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

    /// Whether an instance holding this table should hand it to the partitions and stop keeping one.
    ///
    /// Four things have to hold. There has to be more than one instance, because sharing a table
    /// with nobody is all cost. There has to be no pushed down limit, since that path is refused a
    /// second instance anyway and counts groups against a cap that a partition cannot see. The
    /// aggregate has to be grouped, because an ungrouped one has a single slot and no key to hash.
    /// And the table has to be large enough to be worth the split, which is [`PARTITION_FROM`].
    ///
    /// A crowded budget counts as large enough whatever the group count says. An instance that is
    /// about to be told to spill is better off in the partitions, because sixteen shared tables hold
    /// what N instance tables held and the room that frees may be all that was needed. It also keeps
    /// the ordinary case away from the awkward one: a table that spills before it is handed over has
    /// a file covering every partition, and [`Aggregate::hand_over`] has to drain it row by row.
    fn ought_to_partition(&self, table: &Building) -> bool {
        !self.alone
            && self.max_groups.is_none()
            && self.started.load(Ordering::Relaxed) > 1
            && (table.groups >= PARTITION_FROM || crowded(&self.memory))
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
    /// Three things end it and all three are about room. A table that has opened a spill file can
    /// never be merged with another table, because a key can be in one table and in the other's
    /// file at once and the merge would finish a group the file is still holding rows for. A budget
    /// already half spent is the same crowding the single table path watches for. And the aggregate
    /// itself has to be small enough that one set of tables per instance still fits, which is what
    /// [`Aggregate::room_for_local`] asks.
    ///
    /// Once per chunk rather than once per row, and the answer is almost always yes.
    fn still_local(&self, spreading: &mut Spreading, own: &mut [Option<Building>]) -> Result<bool> {
        // flatten: a partition this instance has not folded into has no table and nothing to say.
        let spilled = own.iter().flatten().any(|table| table.over.is_some());
        if !spilled && !crowded(&self.memory) && self.room_for_local(own) {
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
    fn room_for_local(&self, own: &[Option<Building>]) -> bool {
        let Some(limit) = self.memory.limit() else { return true };
        // flatten: a partition with no table is holding nothing.
        let mine: u64 = own
            .iter()
            .flatten()
            .map(|table| table.scratch.bytes() + table.containers.bytes())
            .sum();
        let instances = self.started.load(Ordering::Relaxed) as u64;
        mine.saturating_mul(instances) < limit / 4
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
        let Spreading { hashes, picks, keyed, spin, .. } = spreading;
        crate::table::hash(&rows.keys, rows.rows, hashes);
        for pick in picks.iter_mut() {
            pick.clear();
        }
        for hashed in keyed.iter_mut() {
            hashed.clear();
        }
        let shift = u64::BITS - RADIX_PARTITIONS.ilog2();
        for (row, &hash) in hashes.iter().enumerate() {
            let partition = (hash >> shift) as usize;
            picks[partition].push(row as u32);
            keyed[partition].push(hash);
        }
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
    /// runs on sixteen threads at once, and the tables it merges are only ever the ones belonging
    /// to a single partition.
    fn spread_own(
        &self,
        rows: &Rows,
        spreading: &mut Spreading,
        own: &mut [Option<Building>],
    ) -> Result<()> {
        // Timed here rather than around each `fold` below, because there are sixteen of those to a
        // chunk and a pair of clock readings on each of them would be a measurable share of what
        // they measure. One reading a chunk is the granularity rule the stage clock is written to.
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

/// The scratch one pipeline instance keeps between chunks, and its table when it has one of its own.
#[derive(Debug)]
pub(crate) struct Partitioned {
    radix_distinct: bool,
    radix_distinct_records: Vec<BigIntDistinctPartition>,
    radix_distinct_memory: Reservation,
    fixed: bool,
    fixed_records: Vec<FixedPartition>,
    fixed_memory: Reservation,
    dense: bool,
    dense_codes: Vec<Vec<u32>>,
    dense_nulls: i64,
    dense_memory: Reservation,
    /// The table this instance folds into while it still keeps its groups to itself.
    ///
    /// Every instance starts with one, because splitting a chunk sixteen ways is not free and a
    /// small aggregate never earns it back. It goes when [`Aggregate::ought_to_partition`] says the
    /// table has grown enough to be worth sharing, and from then on this is `None` and the chunks go
    /// straight into the partitions.
    single: Option<Building>,
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
}

impl Spreading {
    fn new() -> Self {
        Self {
            hashes: Vec::new(),
            picks: vec![Vec::new(); RADIX_PARTITIONS],
            keyed: vec![Vec::new(); RADIX_PARTITIONS],
            spin: 0,
            waiting: Vec::new(),
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

    fn local(&self) -> Partitioned {
        self.started.fetch_add(1, Ordering::Relaxed);
        Partitioned {
            radix_distinct: false,
            radix_distinct_records: (0..RADIX_PARTITIONS)
                .map(|_| BigIntDistinctPartition::default())
                .collect(),
            radix_distinct_memory: self.memory.reservation(),
            fixed: false,
            fixed_records: (0..RADIX_PARTITIONS).map(|_| FixedPartition::default()).collect(),
            fixed_memory: self.memory.reservation(),
            dense: false,
            dense_codes: vec![Vec::new(); DENSE_PARTITIONS],
            dense_nulls: 0,
            dense_memory: self.memory.reservation(),
            single: Some(self.start()),
            expressions: self.inputs.scratch(),
            spreading: Spreading::new(),
            own: (0..RADIX_PARTITIONS).map(|_| None).collect(),
        }
    }

    /// Refused for a limit pushed down into the grouping, and for nothing else.
    ///
    /// A pushed down limit is worse than a refusal, because it would answer. `max_groups` stops the
    /// table opening groups once an unordered limit above cannot observe another, and every
    /// instance would stop at its own tenth group while the rows of the groups it dropped kept
    /// arriving, so `count(*)` would come back short. That is #474's trick, which is worth keeping,
    /// and the price of keeping it is that the aggregate under it runs on one thread.
    ///
    /// Spilling used to be refused here too, because a key could be in one instance's table and in
    /// another instance's spill file at once. Partitioning answers that: a partition's file only
    /// ever holds keys belonging to that partition, so the key is either finished in the partition
    /// or absent from it, which is the invariant spilling rested on all along.
    fn parallel(&self) -> bool {
        self.max_groups.is_none()
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
            radix_distinct,
            radix_distinct_records,
            radix_distinct_memory,
            fixed,
            fixed_records,
            fixed_memory,
            dense,
            dense_codes,
            dense_nulls,
            dense_memory,
            single,
            expressions,
            spreading,
            own,
        } = local;
        let rows = self.read(chunk, expressions)?;
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
            let buffered = self.buffer_fixed(&rows, fixed_records, fixed_memory);
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
        if let Some(table) = single {
            if let Some(error) = table.failure.take() {
                return Err(error);
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
        if self.locally.load(Ordering::Relaxed) && self.still_local(spreading, own)? {
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
    /// The `built` lock is held across all of it. That is what decides the race: an instance reading
    /// the flag and an instance setting it cannot both be between the read and the deposit at once,
    /// so a table is never left whole in partition zero after the switch.
    fn combine(&self, local: Partitioned) -> Result<()> {
        let Partitioned {
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
        if radix_distinct {
            let state = self
                .bigint_distinct
                .get()
                .expect("a distinct exchange exists after a distinct sink");
            for (partition, rows) in radix_distinct_records.iter_mut().enumerate() {
                if rows.rows.is_empty() {
                    continue;
                }
                state.partitions[partition].lock().map_err(poisoned)?.append(rows);
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
                state.partitions[partition].lock().map_err(poisoned)?.append(rows);
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
        let mut kept = self.merged[0].lock().map_err(poisoned)?;
        if kept.table.is_none() {
            kept.table = Some(arriving);
            return Ok(());
        }
        // Two tables cannot be merged when either of them has spilled, because a key can be in one
        // table and in the other's file at once, and the merge would finish a group the file is
        // still holding rows for. Partitioning is the answer to that, so the pair turns it on here
        // rather than the merge refusing. It takes an instance that filled its budget without ever
        // reaching a chunk that would have made it partition on its own, which is rare and used to
        // be a not implemented error.
        let spilled =
            arriving.over.is_some() || kept.table.as_ref().is_some_and(|held| held.over.is_some());
        if spilled {
            let seeded = kept.table.take().expect("the table was there a moment ago");
            built.partitioning = true;
            drop(kept);
            drop(built);
            self.hand_over(seeded, &mut spreading)?;
            return self.hand_over(arriving, &mut spreading);
        }
        self.merge(arriving, kept.table.as_mut().expect("the table was there a moment ago"))?;
        Ok(())
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
    fn finalize(&self) -> Result<()> {
        if let Some(distinct) = self.bigint_distinct.get() {
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<i64>>>> =
                (0..RADIX_PARTITIONS).map(|_| Mutex::new(None)).collect();
            let input = distinct
                .partitions
                .iter()
                .map(|partition| partition.lock().map(|rows| rows.rows.len()).map_err(poisoned))
                .sum::<Result<usize>>()?;
            let degree = input.div_ceil(65_536).clamp(1, RADIX_PARTITIONS);
            let total = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(degree - 1);
                for _ in 1..degree {
                    handles.push(scope.spawn(|| {
                        finish_bigint_distinct(&next, &slots, distinct, &self.memory);
                        stage::here()
                    }));
                }
                finish_bigint_distinct(&next, &slots, distinct, &self.memory);
                let mut theirs = Spent::none();
                for handle in handles {
                    let spent = handle
                        .join()
                        .map_err(|_| Error::internal("a distinct radix worker panicked"))?;
                    theirs.add(spent);
                }
                stage::gained(theirs);
                let mut total = 0_i64;
                for (at, slot) in slots.iter().enumerate() {
                    let count = slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                        Err(Error::internal(format!(
                            "nothing finished distinct radix partition {at}"
                        )))
                    })?;
                    total = total
                        .checked_add(count)
                        .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
                }
                Ok::<_, Error>(total)
            })?;
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
            let bound = self.top_counts.expect("a fixed exchange has a TopN bound");
            let next = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<Result<Part>>>> =
                (0..RADIX_PARTITIONS).map(|_| Mutex::new(None)).collect();
            let parts = std::thread::scope(|scope| {
                let next = &next;
                let slots = &slots;
                let calls = &self.calls;
                let memory = &self.memory;
                let mut handles = Vec::with_capacity(RADIX_PARTITIONS - 1);
                for _ in 1..RADIX_PARTITIONS {
                    handles.push(scope.spawn(move || {
                        finish_fixed(next, slots, fixed, bound, calls, memory);
                        stage::here()
                    }));
                }
                finish_fixed(next, slots, fixed, bound, calls, memory);
                let mut theirs = Spent::none();
                for handle in handles {
                    let spent = handle
                        .join()
                        .map_err(|_| Error::internal("a fixed radix worker panicked"))?;
                    theirs.add(spent);
                }
                stage::gained(theirs);
                let mut parts = Vec::with_capacity(slots.len());
                for (at, slot) in slots.iter().enumerate() {
                    parts.push(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                        Err(Error::internal(format!("nothing finished fixed radix partition {at}")))
                    })?);
                }
                Ok::<_, Error>(parts)
            })?;
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
            let chunks = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(DENSE_PARTITIONS);
                for (number, partition) in dense.partitions.iter().enumerate() {
                    let dictionary = Arc::clone(&dense.dictionary);
                    handles.push(scope.spawn(move || {
                        let mut partition = partition.lock().map_err(poisoned)?;
                        dense_partition(
                            &dictionary,
                            number,
                            &mut partition,
                            &self.constants,
                            group_types,
                        )
                    }));
                }
                let mut chunks = Vec::new();
                for handle in handles {
                    chunks.extend(
                        handle
                            .join()
                            .map_err(|_| Error::internal("a dense count worker panicked"))??,
                    );
                }
                Ok::<_, Error>(chunks)
            })?;
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
        let degree = built.instances.clamp(1, self.merged.len());
        let closed = if degree > 1 { self.close_together(degree)? } else { self.close_in_turn()? };
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

fn bigint_distinct_partition(
    partition: &mut BigIntDistinctPartition,
    memory: &Memory,
) -> Result<i64> {
    const EMPTY: u32 = u32::MAX;
    let capacity = partition.rows.len().saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width_of(capacity * size_of::<u32>()))?;
    let mut buckets = vec![EMPTY; capacity];
    let mask = capacity - 1;
    let mut unique = 0_usize;
    let timing = stage::Timing::start(Stage::Fold);
    for source in 0..partition.rows.len() {
        let row = partition.rows[source];
        let mut at = row.hash as usize & mask;
        loop {
            let slot = buckets[at];
            if slot == EMPTY {
                buckets[at] = u32::try_from(unique)
                    .map_err(|_| Error::out_of_memory("a distinct radix partition is too large"))?;
                partition.rows[unique] = row;
                unique += 1;
                break;
            }
            let held = partition.rows[slot as usize];
            if held.hash == row.hash && held.value == row.value {
                break;
            }
            at = (at + 1) & mask;
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
            .and_then(|mut rows| fixed_partition(&mut rows, bound, calls, memory));
        if let Ok(mut slot) = slots[at].lock() {
            *slot = Some(done);
        }
    }
}

fn fixed_partition(
    partition: &mut FixedPartition,
    bound: usize,
    calls: &[Call],
    memory: &Memory,
) -> Result<Part> {
    const EMPTY: u32 = u32::MAX;
    let capacity = partition.rows.len().saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width_of(
        capacity * size_of::<u32>() + partition.rows.len() * size_of::<CompactNumeric>(),
    ))?;
    let mut buckets = vec![EMPTY; capacity];
    let mut states: Vec<CompactNumeric> = Vec::with_capacity(partition.rows.len());
    let mut overflow = HashMap::new();
    let mask = capacity - 1;
    let all_valid = partition.validity.is_empty();
    let input = partition.rows.len();
    let timing = stage::Timing::start(Stage::Fold);
    for source in 0..input {
        let row = partition.rows[source];
        let valid = if all_valid { FixedRecord::ALL } else { partition.validity[source] };
        let mut at = row.hash as usize & mask;
        let slot = loop {
            let slot = buckets[at];
            if slot == EMPTY {
                let slot = states.len();
                buckets[at] = u32::try_from(slot)
                    .map_err(|_| Error::out_of_memory("a fixed radix partition is too large"))?;
                partition.rows[slot] = row;
                if !all_valid {
                    partition.validity[slot] = valid;
                }
                states.push(CompactNumeric::default());
                break slot;
            }
            let slot = slot as usize;
            let held = partition.rows[slot];
            let held_valid = if all_valid { FixedRecord::ALL } else { partition.validity[slot] };
            if held.hash == row.hash
                && held.first == row.first
                && held.second == row.second
                && held_valid & (FixedRecord::FIRST | FixedRecord::SECOND)
                    == valid & (FixedRecord::FIRST | FixedRecord::SECOND)
            {
                break slot;
            }
            at = (at + 1) & mask;
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
            if valid & FixedRecord::FIRST != 0 { Value::BigInt(key.first) } else { Value::Null },
            if valid & FixedRecord::SECOND != 0 { Value::Integer(key.second) } else { Value::Null },
            Value::BigInt(state.count()),
            Accumulator::exact_sum(sum, state.sum_seen(), &calls[1].returns).finish()?,
            Accumulator::exact_avg(mean, state.mean_count, &calls[2].returns).finish()?,
        ]);
    }
    let mut held = memory.reservation();
    let types = [
        LogicalType::BigInt,
        LogicalType::Integer,
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

impl Aggregate<'_> {
    /// Every partition finished on this thread, which is what one instance means.
    fn close_in_turn(&self) -> Result<Vec<Result<Part>>> {
        Ok((0..self.merged.len()).map(|at| self.close(at)).collect())
    }

    /// Every partition finished across `degree` threads, each thread taking whichever is next.
    ///
    /// The threads are scoped and started here rather than taken from the driver's, because by the
    /// time a sink finalises the driver has already joined every instance and there is nothing else
    /// running. It is the same mechanism the parallel driver uses for the instances themselves.
    ///
    /// The results go into a slot apiece and are read back in partition order, so which thread got
    /// which partition and which finished first change nothing about the answer. That is what makes
    /// this safe to do at all: the rows come out in the order the one thread put them in. A thread
    /// that panics leaves its slot empty, and an empty slot is reported rather than silently
    /// dropping a partition.
    ///
    /// Each of these threads hands its stage clock back on the way out and the thread that started
    /// them adds the readings to its own, so that merging and emitting are charged to the aggregate
    /// that did them. Without it they are charged to nobody: the instrumentation shim reads the
    /// clock on the thread that called `finalize`, these are not that thread, and they are not pool
    /// workers either, so their CPU misses the worker total as well. On ClickBench at a million rows
    /// that was a third of a `GROUP BY URL` sitting in wall time with no counter anywhere to say
    /// what it was.
    fn close_together(&self, degree: usize) -> Result<Vec<Result<Part>>> {
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Result<Part>>>> =
            (0..self.merged.len()).map(|_| Mutex::new(None)).collect();
        let mut theirs = Spent::none();
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(degree - 1);
            for _ in 1..degree {
                handles.push(scope.spawn(|| {
                    self.closing(&next, &slots);
                    // The whole of this thread's reading rather than a difference, because the
                    // thread was made a line ago and has spent nothing else.
                    stage::here()
                }));
            }
            // The thread that asked finishes partitions too rather than waiting on the ones it
            // started, for the reason the parallel driver gives for doing the same.
            self.closing(&next, &slots);
            for handle in handles {
                // A thread that panicked closed no partition, which the empty slot reports below.
                // It also has no reading to add, and losing it matters less than the panic does.
                if let Ok(spent) = handle.join() {
                    theirs.add(spent);
                }
            }
        });
        stage::gained(theirs);
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

    fn finalize(&self) -> Result<()> {
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

    use rudb_common::{Field, LogicalType, Memory, Value};
    use rudb_pipeline::Sink;
    use rudb_plan::{Plan, Slice};
    use rudb_vector::{Chunk, Data, Vector};

    use super::{
        Aggregate, BigIntDistinct, BigIntDistinctPartition, BigIntDistinctRecord, Call,
        CompactNumeric, Distinct, FixedPartition, FixedRecord, bigint_distinct_partition,
        fixed_partition,
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
        distinct.finalize().expect("the answer");

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
        distinct.finalize().expect("the answer");

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
        aggregate.finalize().expect("the answer");

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
        aggregate.finalize().expect("the distinct count");
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
        aggregate.finalize().expect("the zero count");
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
        aggregate.finalize().expect("the answer");

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
        assert!(
            aggregate.built.lock().expect("readable").partitioning,
            "five thousand groups on two instances is meant to take the partitioned path"
        );
        aggregate.finalize().expect("the answer");

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
        aggregate.finalize().expect("the answer");

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
        aggregate.finalize().expect("the answer");

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
        aggregate.finalize().expect("the answer");

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
        distinct.finalize().expect("the answer");

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
    fn a_bigint_radix_partition_counts_unique_values_across_hash_collisions() {
        let mut partition = BigIntDistinctPartition {
            rows: vec![
                BigIntDistinctRecord { hash: 7, value: 11 },
                BigIntDistinctRecord { hash: 7, value: 12 },
                BigIntDistinctRecord { hash: 7, value: 11 },
                BigIntDistinctRecord { hash: 23, value: 13 },
            ],
        };
        assert_eq!(
            bigint_distinct_partition(&mut partition, &Memory::unlimited())
                .expect("the distinct partition"),
            3,
            "equal hashes still compare their integer values"
        );
        assert_eq!(size_of::<BigIntDistinctRecord>(), 16);
    }

    #[test]
    fn fixed_radix_partition_aggregates_collisions_and_nulls_exactly() {
        let mut partition = FixedPartition::default();
        let row = |first, second, sum, mean| FixedRecord { hash: 7, first, second, sum, mean };
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

        let part = fixed_partition(&mut partition, 10, &calls, &Memory::unlimited())
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
                    Value::BigInt(1),
                    Value::Integer(2),
                    Value::BigInt(2),
                    Value::HugeInt(8),
                    Value::Double(4.0),
                ],
                vec![
                    Value::Null,
                    Value::Integer(2),
                    Value::BigInt(2),
                    Value::HugeInt(7),
                    Value::Double(7.0),
                ],
            ]
        );
        assert_eq!(size_of::<FixedRecord>(), 24);
    }
}
