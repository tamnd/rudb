//! Grouping and duplicate elimination.
//!
//! Both are hash tables over [`Key`], which is what makes them agree about what one row is. A
//! `GROUP BY x` that put two nulls in two groups and a `SELECT DISTINCT x` that collapsed them into
//! one would be two answers to the same question, and the only way to be sure that never happens is
//! for both to ask the same type.
//!
//! The table is a `HashMap` from key to slot, and the slot is the number of groups that were seen
//! before this one, so the output comes out in the order the groups were first seen. SQL does not
//! promise that and DuckDB does not either, but a deterministic order costs nothing here and makes a
//! failing test a diff instead of an investigation.
//!
//! The state of every group lives in flat vectors indexed by that slot rather than in a vector of
//! its own, so a group that arrives costs a push and not a trip to the allocator, and the key of a
//! row that is not a new group is written into a buffer this keeps rather than into a new one. What
//! is left per row is the hash and the probe, which is what #237 was about.

use rudb_common::{Error, Field, LogicalType, Memory, Reservation, Result, Value};
use rudb_kernels::{Accumulator, is_true};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Vector};

use crate::expr::{evaluate, evaluate_all};
use crate::key::{Key, RowMap, RowSet};
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// One aggregate call, taken apart once when the operator is built.
#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<ExprRef>,
    distinct: bool,
    filter: Option<ExprRef>,
    returns: LogicalType,
}

/// A grouped or ungrouped aggregation.
///
/// The output is the group expressions followed by the aggregates, which is what a binding into
/// this operator's table index means and what the binder assumed when it made one.
///
/// An ungrouped aggregate produces exactly one row even over an empty input. That is done by
/// creating the single empty group when the operator is built rather than when the first row
/// arrives, which is the whole of the difference between `SELECT count(*) FROM empty` answering
/// zero and answering nothing.
#[derive(Debug)]
pub(crate) struct Aggregate<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    input_schema: Schema,
    groups: Vec<ExprRef>,
    calls: Vec<Call>,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
    memory: Memory,
    /// What the finished chunks are charged, held for as long as this operator holds them.
    held: Reservation,
}

impl<'a> Aggregate<'a> {
    /// An aggregation over the plan's groups and aggregate calls.
    ///
    /// # Errors
    ///
    /// If an entry in the aggregate list is not an aggregate, which [`Plan::validate`] rejects and
    /// which is checked again here because this operator has no sensible behaviour if it is wrong.
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        index: u32,
        groups: Slice,
        aggregates: Slice,
        memory: &Memory,
    ) -> Result<Self> {
        let input_schema = input.schema().clone();
        let groups: Vec<ExprRef> = plan.expr_list(groups).to_vec();
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
            });
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
        Ok(Self {
            input,
            plan,
            input_schema,
            groups,
            calls,
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
            memory: memory.clone(),
            held: memory.reservation(),
        })
    }

    /// Reads the whole input and builds the hash table.
    fn build(&mut self) -> Result<()> {
        // The keys and the rows made out of them, charged separately from the chunks those produce,
        // because these are given back when the last group has been finished and the chunks are
        // not.
        let mut scratch = self.memory.reservation();
        // The three containers and the sets a `DISTINCT` fills, which are gone before the chunks
        // are built rather than after. Their own reservation so that their charge can go when they
        // do, which is what leaves room for the chunks. A key is not in here, because a key is moved
        // into the rows and outlives all of it. Per #272.
        let mut containers = self.memory.reservation();
        let mut charged = 0;
        let mut slots: RowMap<usize> = RowMap::default();
        let mut states: Vec<Accumulator> = Vec::new();
        let mut seen: Vec<RowSet> = Vec::new();
        let calls = self.calls.len();
        // Whether any call is `DISTINCT`, and so whether the sets that answer that are built at all.
        // A group by with a million groups and no `DISTINCT` anywhere in it used to allocate a
        // million empty sets to look at none of them.
        let sets = self.calls.iter().any(|call| call.distinct);
        let alone = self.groups.is_empty();
        let mut groups = 0;
        if alone {
            groups = 1;
            self.fresh(&mut states)?;
            if sets {
                seen.resize_with(calls, RowSet::default);
            }
        }
        // Which calls fold a vector at a time. An ungrouped aggregate has exactly one slot, so
        // there is no key to build, no hash to take and no lookup to do, and what is left of the
        // row loop is the fold itself. `DISTINCT` needs a value per row to put in a set and
        // `FILTER` needs the rows it kept, and neither has a vector form yet, so a call with either
        // stays on the row loop while the calls beside it do not.
        let by_vector: Vec<bool> = self
            .calls
            .iter()
            .map(|call| alone && !call.distinct && call.filter.is_none())
            .collect();
        let every = by_vector.iter().all(|&yes| yes);
        // One row of the group key and one row of arguments per call, filled again for each input
        // row and kept between rows so that the buffers behind them are asked for once and not once
        // per row. Only a row that turns out to be a group nobody has seen is copied out of them.
        let mut key = Key(Vec::new());
        let mut given: Vec<Key> = vec![Key(Vec::new()); calls];
        while let Some(chunk) = self.input.next()? {
            let keys = evaluate_all(self.plan, &self.groups, &self.input_schema, &chunk)?;
            let mut taken = 0;
            let mut aside = 0;
            let mut arguments = Vec::with_capacity(self.calls.len());
            let mut filters = Vec::with_capacity(self.calls.len());
            for call in &self.calls {
                arguments.push(evaluate_all(self.plan, &call.args, &self.input_schema, &chunk)?);
                filters.push(match call.filter {
                    Some(filter) => Some(evaluate(self.plan, filter, &self.input_schema, &chunk)?),
                    None => None,
                });
            }
            for at in 0..calls {
                if by_vector[at] {
                    states[at].update_run(&arguments[at], chunk.len())?;
                }
            }
            if alone && every {
                continue;
            }
            // row at a time: the worst one in the tree, because grouping is where the rows are.
            // 2f (#60) gives it a table that hashes a column at a time and probes a vector at a
            // time, and 2g (#61) gives the aggregate an update that takes a vector and a run of
            // slots, at which point neither the key nor the argument is a `Value` any more.
            for row in 0..chunk.len() {
                let slot = if alone {
                    0
                } else {
                    fill(&mut key, &keys, row);
                    match slots.get(&key) {
                        Some(&slot) => slot,
                        None => {
                            let slot = groups;
                            groups += 1;
                            // A group costs the copy of its key that the table takes, and its own
                            // accumulators and distinct sets in the two vectors beside it. What
                            // those three containers took to have room for all of that is charged
                            // separately, below and once per chunk, because it is a property of the
                            // containers rather than of this group.
                            //
                            // The copy is what gets charged and not the buffer it was copied from,
                            // which is the whole reason it is made before the charge rather than
                            // after. `key` is filled again for every row and a string in it keeps
                            // whatever the longest string it has held needed, so charging that
                            // charges every group in the table for the longest key in the table.
                            let stored = key.clone();
                            taken += rows::heap(&stored.0);
                            slots.insert(stored, slot);
                            self.fresh(&mut states)?;
                            if sets {
                                seen.resize_with(seen.len() + calls, RowSet::default);
                            }
                            slot
                        }
                    }
                };
                for (at, call) in self.calls.iter().enumerate() {
                    if by_vector[at] {
                        continue;
                    }
                    if let Some(flags) = &filters[at] {
                        if !is_true(&flags.value_at(row)) {
                            continue;
                        }
                    }
                    let args = &mut given[at];
                    fill(args, &arguments[at], row);
                    if call.distinct {
                        // Asked before it is added, because the answer is usually that it is there
                        // already and a set that is asked never takes a copy of what it was asked
                        // about. A `count(DISTINCT x)` over a million rows and a thousand values
                        // copies a thousand times rather than a million.
                        let set = &mut seen[slot * calls + at];
                        if set.contains(args) {
                            continue;
                        }
                        // The copy and not the buffer, for the reason the group key above gives.
                        let stored = args.clone();
                        aside += rows::footprint(&stored.0);
                        set.insert(stored);
                    }
                    states[slot * calls + at].update(&args.0)?;
                }
            }
            scratch.grow(taken)?;
            containers.grow(aside)?;
            let now = tables(&slots, &states, &seen);
            rows::capacity(now, &mut charged, &mut containers)?;
        }
        // Turning the table into rows is where this operator holds the most and used to charge the
        // least. `out` is a vector header per group, and each row is the key's own vector with the
        // aggregate results pushed onto it, which asks the allocator for a block wider than the key
        // was given. Both are asked for before they are taken rather than charged after, because
        // the whole of it is taken between one charge and the next and a limit that is told
        // afterwards has not done anything. What a result owns away from itself is not knowable
        // until it has been asked for, so that part is charged as it arrives. Per #272.
        let each = width_of(size_of::<Vec<Value>>() + calls * size_of::<Value>());
        scratch.grow(width_of(groups).saturating_mul(each))?;
        let mut out: Vec<Vec<Value>> = vec![Vec::new(); groups];
        for (key, slot) in slots {
            out[slot] = key.0;
        }
        let mut taken = 0;
        for (slot, row) in out.iter_mut().enumerate() {
            // Room for every result at once, so that the row's block is asked for at the width it
            // ends up at rather than at the width a doubling picks, which is the width charged
            // above.
            row.reserve_exact(calls);
            for accumulator in &states[slot * calls..slot * calls + calls] {
                let value = accumulator.finish()?;
                taken += rows::owned(&value);
                row.push(value);
            }
        }
        scratch.grow(taken)?;
        // The table went with the loop that drained it and the accumulators are finished, so the
        // charge for all of it goes here and not at the end of this function. The chunks are the
        // second copy of the rows and this is the room they are built in.
        drop(states);
        drop(seen);
        containers.release();
        self.chunks = rows::chunks(&self.schema.types(), &out, &mut self.held)?;
        Ok(())
    }

    /// A fresh accumulator per call, appended for the group that has just arrived.
    ///
    /// One flat vector of accumulators rather than a vector per group, so that a new group costs a
    /// push and not a trip to the allocator. The accumulators of the group in `slot` are the run of
    /// `calls` entries starting at `slot * calls`.
    fn fresh(&self, states: &mut Vec<Accumulator>) -> Result<()> {
        for call in &self.calls {
            states.push(Accumulator::new(&call.name, &call.returns)?);
        }
        Ok(())
    }
}

/// Fills `key` with one row of `columns`, reusing what the row before it left behind.
///
/// The point of filling rather than collecting is the strings. A `Value::Varchar` owns its bytes, so
/// reading a string column a row at a time takes a buffer from the allocator on every row and gives
/// it back on the next one, and a group by over `URL` does that a hundred million times to look at
/// each buffer once. Writing into the buffer that is already there asks for nothing. Every other
/// value owns nothing, so overwriting one is a move of a few bytes.
fn fill(key: &mut Key, columns: &[Vector], row: usize) {
    key.0.truncate(columns.len());
    for (at, column) in columns.iter().enumerate() {
        match key.0.get_mut(at) {
            Some(slot) => set(slot, column, row),
            None => key.0.push(column.value_at(row)),
        }
    }
}

/// Puts one column's value at `row` into `slot`, keeping the buffer that is already there if it can.
fn set(slot: &mut Value, column: &Vector, row: usize) {
    if let (Value::Varchar(buffer), Some(text)) = (&mut *slot, column.text_at(row)) {
        buffer.clear();
        buffer.push_str(text);
        return;
    }
    *slot = column.value_at(row);
}

impl Operator for Aggregate<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            self.build()?;
            self.built = true;
        }
        if self.at >= self.chunks.len() {
            return Ok(None);
        }
        let chunk = self.chunks[self.at].clone();
        self.at += 1;
        Ok(Some(chunk))
    }
}

/// What the three containers have taken from the allocator between them.
///
/// Capacity rather than length in all three, which is the point of #227. A `Vec` doubles and so sits
/// between half empty and full, and a `HashMap` fills to seven eighths before doubling as well, so
/// a table of seventeen million groups has paid for somewhere between seventeen and thirty four
/// million slots and the old charge counted seventeen.
///
/// The hash table also has a control byte per bucket beside the buckets themselves, which is how it
/// answers a lookup without touching the keys, and there are more buckets than the capacity it
/// reports. [`rows::buckets`] has that arithmetic.
///
/// What the keys own away from the table is not counted here. That is charged as each group arrives,
/// by [`rows::heap`] over the key, and the two have to divide the group between them without
/// overlapping.
///
/// An accumulator is charged as its own width and not as what it holds. That is a knowing undercount
/// and it is the one left: what a `list()` or a `string_agg()` holds grows with the input and there
/// is no way to ask one how large it has become.
fn tables(slots: &RowMap<usize>, states: &Vec<Accumulator>, seen: &Vec<RowSet>) -> u64 {
    let width = |count: usize, size: usize| {
        u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(width_of(size))
    };
    rows::buckets(slots.capacity()) * (width_of(size_of::<(Key, usize)>()) + 1)
        + width(states.capacity(), size_of::<Accumulator>())
        + width(seen.capacity(), size_of::<RowSet>())
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
#[derive(Debug)]
pub(crate) struct Distinct<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    on: Vec<ExprRef>,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
    memory: Memory,
    /// What the kept chunks are charged, held for as long as this operator holds them.
    held: Reservation,
}

impl<'a> Distinct<'a> {
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        on: Slice,
        memory: &Memory,
    ) -> Self {
        let schema = input.schema().clone();
        Self {
            input,
            plan,
            on: plan.expr_list(on).to_vec(),
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
            memory: memory.clone(),
            held: memory.reservation(),
        }
    }

    fn build(&mut self) -> Result<()> {
        let mut scratch = self.memory.reservation();
        // The table, which is gone before the chunks are built, unlike the rows it decided to keep.
        // Per #272, the same split the aggregate above makes and for the same reason.
        let mut table = self.memory.reservation();
        let mut charged = 0;
        let mut charged_table = 0;
        let mut seen: RowSet = RowSet::default();
        let mut kept: Vec<Vec<Value>> = Vec::new();
        let mut key = Key(Vec::new());
        while let Some(chunk) = self.input.next()? {
            let keys = if self.on.is_empty() {
                Vec::new()
            } else {
                evaluate_all(self.plan, &self.on, &self.schema, &chunk)?
            };
            let mut taken = 0;
            let mut aside = 0;
            // row at a time: `DISTINCT` is a grouping that keeps no aggregate, so it gets its
            // answer from the same table 2f (#60) builds and stops building a key here then.
            for row in 0..chunk.len() {
                if self.on.is_empty() {
                    key.0.clear();
                    key.0.extend(chunk.row(row));
                } else {
                    fill(&mut key, &keys, row);
                }
                // Asked before anything is copied, because a row that has been seen is a row this
                // has no further use for, and most rows of a `DISTINCT` worth running have been.
                if seen.contains(&key) {
                    continue;
                }
                let values: Vec<Value> =
                    if self.on.is_empty() { key.0.clone() } else { chunk.row(row).collect() };
                // The row is kept twice, once as the key in the table and once in the output, and
                // each copy is its own block. What the table and the output took to have room for
                // them is charged below, once per chunk. The copy is charged and not the buffer it
                // came from, for the reason the group key in `build` above gives.
                let stored = key.clone();
                taken += rows::heap(&values);
                aside += rows::heap(&stored.0);
                seen.insert(stored);
                kept.push(values);
            }
            scratch.grow(taken)?;
            table.grow(aside)?;
            let rows = width_of(kept.capacity() * size_of::<Vec<Value>>());
            rows::capacity(rows, &mut charged, &mut scratch)?;
            let now = rows::buckets(seen.capacity()) * (width_of(size_of::<Key>()) + 1);
            rows::capacity(now, &mut charged_table, &mut table)?;
        }
        // The table is not needed to build the chunks and the rows are, so it goes first and its
        // charge goes with it, which is the room the chunks are built in.
        drop(seen);
        table.release();
        self.chunks = rows::chunks(&self.schema.types(), &kept, &mut self.held)?;
        Ok(())
    }
}

impl Operator for Distinct<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            self.build()?;
            self.built = true;
        }
        if self.at >= self.chunks.len() {
            return Ok(None);
        }
        let chunk = self.chunks[self.at].clone();
        self.at += 1;
        Ok(Some(chunk))
    }
}
