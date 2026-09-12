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

use std::sync::Mutex;

use rudb_common::{Error, Field, LogicalType, Memory, Reservation, Result, Value};
use rudb_kernels::{Accumulator, NOWHERE, is_true, update_scattered};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::buffer::Buffered;
use crate::expr::{evaluate, evaluate_all};
use crate::key::{Key, RowSet};
use crate::operator::Operator;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::spill::{Reader, Spill};
use crate::table::{Probe, Table};

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
    /// key would leave a partial state on each side to be merged, and a merge needs a serialize and
    /// a combine per aggregate, which `spec/engine/07-aggregate.md` section 7.8 names as debt not
    /// yet paid. Splitting by key means there is nothing to merge and every aggregate keeps working
    /// unchanged.
    ///
    /// Each pass gives its table and the rows it made back before the next one starts, so what is
    /// carried between passes is the finished chunks and nothing else. A query whose answer on its
    /// own fills the budget still runs out, which is correct: there is no way to hold seventeen
    /// million rows in room that does not hold them.
    fn build(&mut self) -> Result<()> {
        // Borrowed field by field rather than as `&self`, because the first pass holds the input
        // out of the same struct and the two borrows have to be disjoint.
        let pass = Pass {
            plan: self.plan,
            input_schema: &self.input_schema,
            schema: &self.schema,
            groups: &self.groups,
            calls: &self.calls,
            memory: &self.memory,
        };
        let mut source = Source::Input(&mut *self.input);
        let mut left = pass.once(&mut source, &mut self.chunks, &mut self.held)?;
        while let Some(mut file) = left {
            // The file is read back through the same `once` the input went through, so there is one
            // row loop, one table and one set of charges however many passes a query takes.
            let mut source = Source::Spilled(Spilled::new(file.read()?, pass.spilled_types()));
            left = pass.once(&mut source, &mut self.chunks, &mut self.held)?;
        }
        Ok(())
    }
}

/// The parts of an [`Aggregate`] one pass over the rows needs.
///
/// A struct of borrows rather than a method on the operator, so that the first pass can hold the
/// input operator mutably while the pass holds everything else.
struct Pass<'p> {
    plan: &'p Plan,
    input_schema: &'p Schema,
    schema: &'p Schema,
    groups: &'p [ExprRef],
    calls: &'p [Call],
    memory: &'p Memory,
}

impl Pass<'_> {
    /// One pass: fill a table until it cannot take another group, then spill what is left.
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
    fn once(
        &self,
        source: &mut Source<'_, '_>,
        chunks: &mut Vec<Chunk>,
        held: &mut Reservation,
    ) -> Result<Option<Spill>> {
        // The keys and the rows made out of them, given back when this pass ends, because by then
        // they are in the chunks.
        let mut scratch = self.memory.reservation();
        // The three containers and the sets a `DISTINCT` fills, which are gone before the chunks
        // are built rather than after. Their own reservation so that their charge can go when they
        // do, which is what leaves room for the chunks. A key is not in here, because a key is moved
        // into the rows and outlives all of it. Per #272.
        let mut containers = self.memory.reservation();
        let mut charged = 0;
        // What the keys the table has taken a copy of own away from themselves, charged against the
        // scratch rather than against the containers because those strings move into the rows and
        // outlive the table. `charged` and this one are the same arrangement over two reservations.
        let mut charged_keys = 0;
        let mut table = Table::new(self.groups.len());
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
        // One row of arguments per call, filled again for each input row and kept between rows so
        // that the buffers behind them are asked for once and not once per row. Only a row that
        // turns out to be new to a `DISTINCT` is copied out of one.
        let mut given: Vec<Key> = vec![Key(Vec::new()); calls];
        // One hash per row of the chunk in hand, built a column at a time before the row loop
        // starts. Kept between chunks for the reason the buffers above are.
        let mut hashes: Vec<u64> = Vec::new();
        // One slot per row of the chunk in hand, which is what the probe produces and what the
        // scatter consumes, and the same slots with a call's `FILTER` folded into them.
        let mut slots: Vec<usize> = Vec::new();
        let mut kept: Vec<usize> = Vec::new();
        // The file the rows that do not fit go to, made the first time the budget says the table
        // has to stop growing and `None` for as long as it does not. One row of it, kept between
        // rows so that writing does not go to the allocator per row.
        let mut over: Option<Spill> = None;
        let mut away: Vec<Value> = Vec::new();
        while let Some(seen_rows) = source.next(self)? {
            let Rows { keys, arguments, filters, rows: length } = &seen_rows;
            let mut aside = 0;
            for at in 0..calls {
                if by_vector[at] {
                    states[at].update_run(&arguments[at], *length)?;
                }
            }
            if alone && every {
                continue;
            }
            // The column at a time half of #237. One pass over each key column turns the whole
            // chunk into one hash per row, with the type of the column matched on once rather than
            // once per value, and the row loop below is then a probe with the hash already in hand.
            if !alone {
                crate::table::hash(keys, *length, &mut hashes);
            }
            // The probe, and nothing else. What comes out of it is one slot per row, which is what
            // the scatter below needs and what the row loop used to consume as it went.
            slots.clear();
            slots.resize(*length, NOWHERE);
            for row in 0..*length {
                slots[row] = if alone {
                    0
                } else {
                    match table.probe(hashes[row], keys, row) {
                        Probe::Found(slot) => slot,
                        Probe::Vacant(bucket) => {
                            if let Some(file) = over.as_mut() {
                                // The table is as large as the budget will let it be and this key
                                // is not in it, so the row goes out whole. Every later row with
                                // this key goes out too, because the key is never inserted here,
                                // and that is what lets the next pass finish the group without
                                // knowing anything about this one.
                                put_away(file, &seen_rows, row, &mut away)?;
                                continue;
                            }
                            // A group costs the copy of its key that the table takes, and its own
                            // accumulators and distinct sets in the two vectors beside it. What all
                            // of those took to have room for it is charged below and once per
                            // chunk, because it is a property of the containers rather than of this
                            // group, and what the key owns away from itself the table adds up as it
                            // goes and is charged the same way.
                            let slot = table.insert(bucket, hashes[row], keys, row)?;
                            groups = table.len();
                            self.fresh(&mut states)?;
                            if sets {
                                seen.resize_with(seen.len() + calls, RowSet::default);
                            }
                            slot
                        }
                    }
                };
            }
            // The aggregate half of #61. Every call that is not `DISTINCT` folds the whole chunk in
            // one pass, with the aggregate and the layout of its argument matched on once for the
            // chunk rather than once per row, and with no `Value` built at all on the paths the
            // kernel covers.
            for (at, call) in self.calls.iter().enumerate() {
                if by_vector[at] {
                    continue;
                }
                if call.distinct {
                    aside +=
                        self.distinct(&mut states, &mut seen, &seen_rows, &slots, at, &mut given)?;
                    continue;
                }
                let picked = match &filters[at] {
                    None => &slots,
                    Some(flags) => {
                        // A row the filter dropped belongs to nothing, which is the same thing the
                        // scatter already understands a spilled row to be, so the filter goes into
                        // the slots rather than into the loop that reads them.
                        kept.clear();
                        kept.extend(slots.iter().enumerate().map(|(row, &slot)| {
                            if slot != NOWHERE && is_true(&flags.value_at(row)) {
                                slot
                            } else {
                                NOWHERE
                            }
                        }));
                        &kept
                    }
                };
                update_scattered(&mut states, picked, calls, at, arguments[at].first(), *length)?;
            }
            rows::capacity(table.owned(), &mut charged_keys, &mut scratch)?;
            containers.grow(aside)?;
            let now = tables(&table, &states, &seen);
            rows::capacity(now, &mut charged, &mut containers)?;
            // Asked after the chunk has been folded in and not before, so that a pass always takes
            // at least one chunk of groups whatever the budget says. That is what makes the loop in
            // `build` finish: a pass that could spill from its first row would spill every row and
            // hand back a file the same size as what it was given.
            match over.as_ref() {
                None if !alone && crowded(self.memory) => {
                    over = Some(Spill::new("aggregate", self.spilled_types())?);
                }
                Some(file) => hopeless(file, groups)?,
                None => {}
            }
        }
        // The distinct sets are finished with and the table and the accumulators are not, so the
        // charge for the sets goes here rather than after the chunks are built, which is part of
        // the room the chunks are built in.
        drop(seen);
        let alive = table.footprint() + width_of(states.capacity() * size_of::<Accumulator>());
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
        // The one buffer the results of a call go through on their way into a vector, kept between
        // chunks and charged once.
        scratch.grow(width_of(VECTOR_SIZE.min(groups) * size_of::<Value>()))?;
        let mut results: Vec<Value> = Vec::new();
        // row at a time: the outer loop steps a chunk at a time and the key columns are copied a
        // column at a time out of the table, so the only thing left here that is per group is asking
        // each accumulator for its result, which is 2g (#61).
        for start in (0..groups).step_by(VECTOR_SIZE) {
            let end = (start + VECTOR_SIZE).min(groups);
            let mut columns = Vec::with_capacity(width + calls);
            for (at, ty) in types.iter().take(width).enumerate() {
                columns.push(Vector::from_values(ty.clone(), &table.column(at)[start..end])?);
            }
            for (at, ty) in types.iter().skip(width).enumerate() {
                // What a result owns away from itself is not knowable until it has been asked for,
                // so that part is charged as it arrives and given back once it is in the vector.
                let mut taken = 0;
                results.clear();
                for slot in start..end {
                    let value = states[slot * calls + at].finish()?;
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
        seen: &mut [RowSet],
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
            let args = &mut given[at];
            fill(args, &rows.arguments[at], row);
            // Asked before it is added, because the answer is usually that it is there already and
            // a set that is asked never takes a copy of what it was asked about. A
            // `count(DISTINCT x)` over a million rows and a thousand values copies a thousand times
            // rather than a million.
            let set = &mut seen[slot * calls + at];
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
    fn fresh(&self, states: &mut Vec<Accumulator>) -> Result<()> {
        for call in self.calls {
            states.push(Accumulator::new(&call.name, &call.returns)?);
        }
        Ok(())
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
        for &group in self.groups {
            types.push(self.plan.expr_type(group).clone());
        }
        for call in self.calls {
            for &argument in &call.args {
                types.push(self.plan.expr_type(argument).clone());
            }
        }
        for call in self.calls {
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
    /// How many columns one of these rows is written out as, which is [`Pass::spilled_types`] long.
    fn width(&self) -> usize {
        self.keys.len()
            + self.arguments.iter().map(Vec::len).sum::<usize>()
            + self.filters.iter().flatten().count()
    }
}

/// Where the rows a pass folds are coming from.
///
/// The first pass reads the operator below it and every pass after that reads the file the pass
/// before it wrote. Writing it as one enum with one `next` rather than as two loops is the whole
/// reason the table, the row loop, the `DISTINCT` sets and the memory charging are written once:
/// neither case knows which one it is.
enum Source<'s, 'o> {
    Input(&'s mut (dyn Operator + 'o)),
    Spilled(Spilled<'s>),
}

impl Source<'_, '_> {
    /// The next chunk of rows, or `None` at the end of the input or the file.
    fn next(&mut self, pass: &Pass<'_>) -> Result<Option<Rows>> {
        match self {
            Source::Input(input) => {
                let Some(chunk) = input.next()? else {
                    return Ok(None);
                };
                let rows = chunk.len();
                let keys = evaluate_all(pass.plan, pass.groups, pass.input_schema, &chunk)?;
                let mut arguments = Vec::with_capacity(pass.calls.len());
                let mut filters = Vec::with_capacity(pass.calls.len());
                for call in pass.calls {
                    arguments.push(evaluate_all(pass.plan, &call.args, pass.input_schema, &chunk)?);
                    filters.push(match call.filter {
                        Some(filter) => {
                            Some(evaluate(pass.plan, filter, pass.input_schema, &chunk)?)
                        }
                        None => None,
                    });
                }
                Ok(Some(Rows { keys, arguments, filters, rows }))
            }
            Source::Spilled(spilled) => spilled.next(pass),
        }
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

impl<'s> Spilled<'s> {
    fn new(reader: Reader<'s>, types: Vec<LogicalType>) -> Self {
        let columns = vec![Vec::new(); types.len()];
        Self { reader, types, row: Vec::new(), columns }
    }

    /// Up to [`VECTOR_SIZE`] rows, turned back into the vectors they were written out of.
    fn next(&mut self, pass: &Pass<'_>) -> Result<Option<Rows>> {
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
        let keys: Vec<Vector> = taking.by_ref().take(pass.groups.len()).collect();
        let mut arguments = Vec::with_capacity(pass.calls.len());
        for call in pass.calls {
            arguments.push(taking.by_ref().take(call.args.len()).collect());
        }
        let mut filters = Vec::with_capacity(pass.calls.len());
        for call in pass.calls {
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
            Some(slot) => set(slot, column, row),
            None => away.push(column.value_at(row)),
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
fn tables(table: &Table, states: &Vec<Accumulator>, seen: &Vec<RowSet>) -> u64 {
    let width = |count: usize, size: usize| {
        u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(width_of(size))
    };
    table.footprint()
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
                local.key.0.extend(chunk.row(row));
            } else {
                fill(&mut local.key, &keys, row);
            }
            // Asked before anything is copied, because a row that has been seen is a row this has
            // no further use for, and most rows of a `DISTINCT` worth running have been.
            if local.seen.contains(&local.key) {
                continue;
            }
            let values: Vec<Value> =
                if self.whole { local.key.0.clone() } else { chunk.row(row).collect() };
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
    use rudb_common::{Field, LogicalType, Memory, Value};
    use rudb_pipeline::Sink;
    use rudb_plan::{Plan, Slice};
    use rudb_vector::{Chunk, Data, Vector};

    use super::Distinct;
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

    #[test]
    fn a_distinct_over_nothing_produces_nothing() {
        let (distinct, out) = distinct();
        distinct.combine(distinct.local()).expect("an instance that saw no chunks");
        distinct.finalize().expect("the answer");

        assert_eq!(out.len().expect("readable"), 0);
    }
}
