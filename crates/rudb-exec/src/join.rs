//! Joins.
//!
//! Two ways of finding the rows one row matches, and one set of rules about what to do with them.
//!
//! A condition that is an equality between a column of one side and a column of the other is
//! answered by looking the value up. The gathered side goes into a hash table once and each driving
//! row reads one entry out of it, so the work is the two sides added rather than multiplied. That
//! is the asymptotic half of `spec/07-execution.md` section 7.4 and it is where every join in a
//! query past the simplest one ends up.
//!
//! Anything else is a nested loop: the condition is evaluated over the driving row paired with a
//! whole chunk of the gathered side, which keeps the evaluator on its batch interface and makes the
//! driving row's columns constant vectors that cost one value each. It is the only answer for a
//! range condition or a call, and it is what the equality path is read against, since eight join
//! kinds each have their own rule about a row with no match and the way to find out whether the
//! faster path has them right is to run both and diff.
//!
//! The rules themselves are written once, below the split. Both paths produce the same thing, a
//! list of positions in the gathered side in the order that side holds them, and everything that
//! decides what a kind does with that list reads it without knowing which path made it.
//!
//! # Two operators
//!
//! There are two types here doing the join rather than one, and what separates them is not the
//! condition but the kind. [`Probe`] is the lookup written as a [`Stream`]: it holds the table and
//! nothing else, answers a driving chunk into an output chunk, and hands that chunk downstream
//! before it looks at the next one. [`Join`] is a [`Sink`]: it gathers the whole driving side,
//! answers all of it in `finalize`, and holds the whole answer until something reads it.
//!
//! The reason both exist is that five of the eight kinds decide about a driving row from that row's
//! own matches, and three do not. A `RIGHT` or a `FULL` join has to produce the gathered rows
//! nothing matched, and which those are is not known until the last driving row has been through. A
//! `MARK` join asks a question of the whole gathered side per driving row, and its answer
//! distinguishes no match from a match nobody could decide, which no list of positions carries. A
//! `POSITIONAL` join is not a lookup at all. Those three stay on the sink, along with every join
//! whose condition a lookup cannot answer, and [`streamed`] is where the line is drawn.
//!
//! What the lookup does not share with the grouping next door is the null rule. A group key answers
//! `IS NOT DISTINCT FROM`, where two nulls are one key, and `=` answers null for a null on either
//! side, so a row whose key holds a null takes part in no pair. The key encoding is the same one and
//! the rows that would hash to a null key are left out of both the table and the probe, which is the
//! whole of the difference and is the one place these two are easy to quietly unify.
//!
//! # Two pipelines and an edge
//!
//! A join is two inputs, and two inputs is two pipelines with a dependency between them. One side
//! ends in a [`Gather`](crate::gather::Gather), which keeps its rows and does nothing else, and the
//! other ends here. The order is not a choice: no row of the second side can be answered until every
//! row of the first it might match has been seen, and that is the edge the scheduler will read off
//! the plan. It is the edge the lookup above builds on too: the gathered side is the side the table
//! is built from, and the table can be built because that pipeline has finished.
//!
//! Which of the plan's two inputs is gathered is `build` on `Node::Join`, chosen by `rudb_opt`'s
//! `sides` pass from a cardinality estimate. Everything below is written as though the gathered side
//! were the right one, because when it is not, `crates/rudb-exec/src/build.rs` hands this operator
//! the two inputs the other way round and the mirror of the plan's join kind. `LEFT` with its inputs
//! swapped is `RIGHT`, which is a kind the rules below already have, so the swap costs no second
//! spelling of any rule. What it does cost is the column order: the operator produces its own left
//! side's columns first and the plan asked for the plan's left side first, so [`Join::swapped`] is
//! set and [`Sink::finalize`] puts the two halves back before anything downstream sees them.
//!
//! What the gathered side costs is the one cost a join cannot get out of. The table is the reason
//! the answer is cheap and there is no table until the rows are in it, so a join is a pipeline
//! breaker on that side and always will be. The driving side is a different matter and used to be
//! held for no reason at all, which is what [`Probe`] is: on the kinds that allow it, a join now
//! costs one side rather than two sides and the answer, which is the memory half of #211 and is what
//! stood between the cross product rewrite and coming back.

use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{
    Cancel, Error, LogicalType, Memory, Reservation, Result, Session, SessionTimeZone, Value,
};
use rudb_kernels::{Connective, combine, is_true};
use rudb_pipeline::{Progress, Sink, Stream};
use rudb_plan::{CompareOp, Expr, ExprRef, JoinKind, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::buffer::Buffered;
use crate::expr::evaluate_all_in_time_zone;
use crate::gather::{self, Gathering, Rows};
use crate::key::{Key, RowMap};
use crate::rows;
use crate::schema::Schema;

/// A join with a condition.
#[derive(Debug)]
pub(crate) struct Join<'a> {
    plan: &'a Plan,
    kind: JoinKind,
    conditions: Vec<ExprRef>,
    marker: Option<usize>,
    left_schema: Schema,
    right_schema: Schema,
    combined: Schema,
    schema: Schema,
    /// Whether this operator's left side is the plan's right one.
    ///
    /// Read once per output chunk, in [`Sink::finalize`], where it says to put the two halves of
    /// every row back the way the plan asked for them. Nothing else in the operator looks at it:
    /// the conditions are resolved through [`Schema::position_of`], which searches by binding
    /// rather than counting columns, so a condition over either side finds its columns wherever
    /// they ended up.
    swapped: bool,
    memory: Memory,
    /// The same token the `cancel` module wraps every node in, held here as well.
    ///
    /// This is the one operator that needs it. The wrapper checks between chunks and the whole
    /// nested loop happens inside one call to `finalize`, so a loop over a hundred thousand left
    /// rows and thirty thousand right ones runs for a minute with nothing looking at the token.
    cancel: Cancel,
    /// The right side, filled by the pipeline this one depends on.
    right: Rows,
    /// The left side, as every instance gathered it.
    left: Mutex<Vec<Vec<Value>>>,
    /// What the left side is charged, given back once the finished chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the joined chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
    /// The parsed zone used by casts in join conditions.
    time_zone: SessionTimeZone,
}

/// The side of a join that is finished before the other one starts.
///
/// The schema and the rows travel together because they are one thing, which is what the pipeline
/// on the other end of the dependency edge produced. When the hash join arrives this is where its
/// table goes.
pub(crate) struct Gathered<'s> {
    /// What that side's rows look like.
    pub(crate) schema: &'s Schema,
    /// The rows, readable once the pipeline that filled them has finished.
    pub(crate) rows: Rows,
    /// The marker's position for a mark join, when it is not the final column.
    pub(crate) marker: Option<usize>,
    /// Whether this is the plan's left input rather than its right one.
    ///
    /// Here rather than beside the kind because it is a fact about which side was gathered, which
    /// is the one thing this type is about. What reads it is the column order of the answer.
    pub(crate) swapped: bool,
}

impl<'a> Join<'a> {
    /// The sink for the driving side, and the source the answer comes out of.
    ///
    /// `left` is the schema of the side whose rows arrive here and `right` is the side the pipeline
    /// before this one gathered. `kind` is stated in those terms too, which is the plan's kind
    /// mirrored when `swapped`.
    ///
    /// [`Gathered::swapped`] says the caller handed the plan's two inputs over the other way round,
    /// which only changes the order the columns come out in. A kind with no mirror is never
    /// swapped, so the kinds that throw one side's columns away are only ever seen the way the plan
    /// wrote them.
    pub(crate) fn new(
        plan: &'a Plan,
        left: &Schema,
        right: Gathered<'_>,
        kind: JoinKind,
        conditions: Slice,
        cancel: &Cancel,
        memory: &Memory,
    ) -> (Self, Buffered) {
        let swapped = right.swapped;
        let combined = Schema::concat(left, right.schema);
        let schema = match kind {
            JoinKind::Semi | JoinKind::Anti => left.clone(),
            // The plan's order rather than this operator's. What is downstream was built against
            // the plan and asks for its columns by binding, and a binding that resolves to the
            // wrong position is the one way this operator can be wrong without failing.
            _ if swapped => Schema::concat(right.schema, left),
            _ => combined.clone(),
        };
        let out = Buffered::new();
        let marker = if kind == JoinKind::Mark {
            right.marker.or_else(|| right.schema.bindings().len().checked_sub(1))
        } else {
            None
        };
        let join = Self {
            plan,
            kind,
            conditions: plan.expr_list(conditions).to_vec(),
            marker,
            left_schema: left.clone(),
            right_schema: right.schema.clone(),
            combined,
            schema,
            swapped,
            memory: memory.clone(),
            cancel: cancel.clone(),
            right: right.rows,
            left: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
            time_zone: SessionTimeZone::default(),
        };
        (join, out)
    }

    /// Applies the session semantics to join conditions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.time_zone = session.session_time_zone();
        self
    }

    /// What this operator produces, which is both sides' columns unless the kind throws one away.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The joined rows, before they are turned back into chunks.
    fn joined(
        &self,
        left_rows: &[Vec<Value>],
        right_rows: &[Vec<Value>],
    ) -> Result<Vec<Vec<Value>>> {
        // The rows being paired up, charged apart from the chunks that come out, because a nested
        // loop join holds all of it at once and gives back everything but the output when it is
        // done.
        let mut scratch = self.memory.reservation();
        let left_types = self.left_schema.types();
        let right_types = self.right_schema.types();
        if self.kind == JoinKind::Positional {
            return Ok(positional(left_rows, right_rows, left_types.len(), right_types.len()));
        }
        // A mark join asks a question about the whole of the gathered side rather than collecting
        // the rows that match, and its answer distinguishes no match from a match nobody could
        // decide, so it stays on the loop until it has a rule of its own.
        let equalities = if self.kind == JoinKind::Mark {
            None
        } else {
            equalities(self.plan, &self.conditions, &self.left_schema, &self.right_schema)
        };
        let index = match &equalities {
            Some(equalities) => {
                Some(index(right_rows, &equalities.right, &self.cancel, &mut scratch)?)
            }
            None => None,
        };
        // Nothing evaluates a condition over the gathered side when the index answers it, and these
        // chunks are a second copy of a side that is already held whole.
        let right_chunks = match index {
            Some(_) => Vec::new(),
            None => rows::chunks(&right_types, right_rows, &mut scratch)?,
        };
        let mut matched = vec![false; right_rows.len()];
        scratch.grow(u64::try_from(right_rows.len()).unwrap_or(u64::MAX))?;
        let mut out: Vec<Vec<Value>> = Vec::new();
        for left_row in left_rows {
            // Once per left row, in the same place and for the same reason as the reservation at
            // the bottom of the loop. What a query can run past its clock by is one pass over the
            // right side, which is the smallest unit this loop has that is not the inner one.
            self.cancel.check()?;
            let before = out.len();
            if self.kind == JoinKind::Mark {
                let marker = self.marker(left_row, &left_types, &right_chunks)?;
                let mut row = pad_right(left_row, right_types.len());
                let Some(position) = self.marker else {
                    return Err(Error::internal("a mark join has no marker column"));
                };
                row[left_types.len() + position] = marker;
                out.push(row);
                scratch.grow(out[before..].iter().map(|row| rows::footprint(row)).sum())?;
                continue;
            }
            let scanned;
            let hits: &[usize] = match (&equalities, &index) {
                (Some(equalities), Some(index)) => hits(index, left_row, &equalities.left),
                _ => {
                    scanned = self.matching(left_row, &left_types, &right_chunks)?;
                    &scanned
                }
            };
            for &hit in hits {
                matched[hit] = true;
            }
            match self.kind {
                JoinKind::Mark => unreachable!("mark joins leave before collecting hits"),
                JoinKind::Semi => {
                    if !hits.is_empty() {
                        out.push(left_row.clone());
                    }
                }
                JoinKind::Anti => {
                    if hits.is_empty() {
                        out.push(left_row.clone());
                    }
                }
                JoinKind::Single => {
                    if hits.len() > 1 {
                        return Err(too_many_rows());
                    }
                    match hits.first() {
                        Some(&hit) => out.push(pair(left_row, &right_rows[hit])),
                        None => out.push(pad_right(left_row, right_types.len())),
                    }
                }
                JoinKind::Left | JoinKind::Full if hits.is_empty() => {
                    out.push(pad_right(left_row, right_types.len()));
                }
                _ => {
                    for &hit in hits {
                        out.push(pair(left_row, &right_rows[hit]));
                    }
                }
            }
            // Once per left row rather than once per output row, so the amount a query can pass
            // its limit by before it is told is one left row's worth of output, which is at most
            // the right side. That is the granularity the loop gives without a check inside the
            // inner one, and a join that produces more than the limit from a single left row is a
            // join whose right side was already over it.
            scratch.grow(out[before..].iter().map(|row| rows::footprint(row)).sum())?;
        }
        if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            for (at, seen) in matched.iter().enumerate() {
                if !seen {
                    out.push(pad_left(left_types.len(), &right_rows[at]));
                }
            }
        }
        Ok(out)
    }

    /// The right side rows one left row matches, by position in the right side.
    ///
    /// No conditions means everything matches, which for an inner join is a cross product and for
    /// an outer one is not, and that difference is why a join with no conditions is a different
    /// node from a [`CrossProduct`].
    fn matching(
        &self,
        left_row: &[Value],
        left_types: &[LogicalType],
        right_chunks: &[Chunk],
    ) -> Result<Vec<usize>> {
        let mut hits = Vec::new();
        let mut base = 0;
        for chunk in right_chunks {
            let rows = chunk.len();
            if self.conditions.is_empty() {
                hits.extend(base..base + rows);
            } else {
                let combined = widen(left_row, left_types, chunk)?;
                let flags = evaluate_all_in_time_zone(
                    self.plan,
                    &self.conditions,
                    &self.combined,
                    &combined,
                    self.time_zone,
                )?;
                let merged = combine(Connective::And, &flags)?;
                // row at a time: this is the nested loop, which is what a condition no lookup can
                // answer still gets. The flags are already a vector here, so what this wants is the
                // selection that 2c (#57) threads.
                for row in 0..rows {
                    if is_true(&merged.value_at(row)) {
                        hits.push(base + row);
                    }
                }
            }
            base += rows;
        }
        Ok(hits)
    }

    /// SQL's `ANY` result: true for a hit, null for no hit with an unknown comparison, false
    /// otherwise. The right side is scanned without building the cross product a rewrite through
    /// an ordinary inner join would materialise.
    fn marker(
        &self,
        left_row: &[Value],
        left_types: &[LogicalType],
        right_chunks: &[Chunk],
    ) -> Result<Value> {
        let mut unknown = false;
        for chunk in right_chunks {
            let combined = widen(left_row, left_types, chunk)?;
            let flags = evaluate_all_in_time_zone(
                self.plan,
                &self.conditions,
                &self.combined,
                &combined,
                self.time_zone,
            )?;
            let merged = combine(Connective::And, &flags)?;
            // row at a time: a mark join stops at the first true value and otherwise remembers
            // whether any lane was null, which cannot be reduced by the ordinary boolean kernel.
            for row in 0..chunk.len() {
                match merged.value_at(row) {
                    Value::Boolean(true) => return Ok(Value::Boolean(true)),
                    Value::Null => unknown = true,
                    _ => {}
                }
            }
        }
        Ok(if unknown { Value::Null } else { Value::Boolean(false) })
    }
}

impl Sink for Join<'_> {
    type Local = Gathering;

    fn local(&self) -> Gathering {
        gather::gathering(&self.memory)
    }

    /// Not yet. The nested loop walks the left rows in the order they were gathered, so the answer
    /// comes out in that order, and two instances gather in whichever order they were scheduled.
    /// The hash join in #62 is what stops that mattering.
    fn parallel(&self) -> bool {
        false
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathering) -> Result<Progress> {
        gather::take(chunk, local)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathering) -> Result<()> {
        let (rows, charged) = gather::into_parts(local);
        self.left.lock().map_err(poisoned)?.extend(rows);
        self.charged.lock().map_err(poisoned)?.push(charged);
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        let left_rows = std::mem::take(&mut *self.left.lock().map_err(poisoned)?);
        let right_rows = self.right.take()?;
        let mut out = self.joined(&left_rows, &right_rows)?;
        if self.swapped {
            // Back into the plan's order. Every row here is this operator's left half followed by
            // its right half, and the plan asked for the other way round, so one rotation by the
            // width of the half that is in front puts each row right. In place and on the rows
            // rather than on the chunks, because the rows are already owned and the chunks are not
            // built until the next line.
            let width = self.left_schema.width();
            for row in &mut out {
                row.rotate_left(width);
            }
        }
        // Both sides go before the answer is built, because the answer is as large as both of them
        // together and holding three copies is what the budget exists to stop.
        drop(left_rows);
        drop(right_rows);
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.schema.types(), &out, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a join gathered")
}

/// An unconditional cross product.
///
/// The only join shaped operator here that does not hold its left side. It holds the right side,
/// because that one has to be replayed once per left row, and then walks the left a row at a time
/// emitting one combined chunk per right chunk. A cross product of a thousand by a thousand is a
/// million rows and there is no way around producing them, but there is a way around holding them
/// all at once and this is it.
///
/// That is also why it is a [`Stream`] rather than a [`Sink`]. One input chunk becomes as many
/// output chunks as the right side has, and what says so is [`Progress::Again`]: the driver hands
/// each one on and asks for the next. The left chunk being walked lives in the instance state,
/// because the chunk the driver hands back on the next call holds whatever the operators below left
/// in it.
///
/// The right side is kept as the chunks it arrived in, by a [`Keep`](crate::gather::Keep) at the end
/// of the pipeline this one depends on. Rows would have to be built back into chunks here, once, for
/// nothing.
#[derive(Debug)]
pub(crate) struct CrossProduct {
    left_types: Vec<LogicalType>,
    types: Vec<LogicalType>,
    schema: Schema,
    /// The right side, filled by the pipeline this one depends on.
    right: Buffered,
}

/// Where one instance of a cross product is in the left chunk it was given.
#[derive(Debug)]
pub(crate) struct Crossing {
    /// The left chunk being walked, held while there is any of it left to pair.
    left: Option<Chunk>,
    row: usize,
    at: usize,
}

impl CrossProduct {
    /// `right` is the handle on the chunks the other pipeline kept.
    pub(crate) fn new(left: &Schema, right_schema: &Schema, right: Buffered) -> Self {
        let schema = Schema::concat(left, right_schema);
        Self { left_types: left.types(), types: schema.types(), schema, right }
    }

    /// What this operator produces, which is both sides' columns.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Stream for CrossProduct {
    type Local = Crossing;

    fn local(&self) -> Crossing {
        Crossing { left: None, row: 0, at: 0 }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Crossing) -> Result<Progress> {
        let stored = self.right.len()?;
        let left = match local.left.take() {
            // Being asked again, so the chunk holds whatever was downstream of it and the left side
            // is the one this instance kept.
            Some(left) => left,
            None => {
                local.row = 0;
                local.at = 0;
                chunk.clone()
            }
        };
        if stored == 0 || left.is_empty() {
            *chunk = Chunk::empty(&self.types);
            return Ok(Progress::More);
        }
        let right = self
            .right
            .at(local.at)?
            .ok_or_else(|| Error::internal("a cross product asked for a chunk nobody kept"))?;
        let row: Vec<Value> = left.row(local.row).collect();
        *chunk = widen(&row, &self.left_types, &right)?;
        local.at += 1;
        if local.at >= stored {
            local.at = 0;
            local.row += 1;
        }
        if local.row >= left.len() {
            return Ok(Progress::More);
        }
        local.left = Some(left);
        Ok(Progress::Again)
    }
}

/// A join answered by looking rows up, a chunk of the driving side at a time.
///
/// This is the same answer [`Join`] gives and a different shape to give it in. The gathered side
/// goes into a table once, and then a driving row is answered by reading one entry out of that
/// table, which is a decision about that row and nothing else. Nothing about it needs the rows that
/// came before it or the rows that come after it, so nothing has to be held to make it, and an
/// operator that holds nothing is a [`Stream`] rather than a [`Sink`].
///
/// Three things follow from that and all three are the point. The driving side is never gathered, so
/// a join over a billion row scan costs the table and not the scan. The answer leaves as it is
/// produced rather than being collected and handed on at the end, so a join that produces more rows
/// than fit in memory is a join that runs. And the driving pipeline is no longer pinned to one
/// thread by a sink that says [`Sink::parallel`] is false: every instance reads the same table and
/// writes nobody's state, so the scan underneath can use every thread there is. The order the rows
/// come out in is then whichever instance got there first, which is an order no join ever promised
/// and is what DuckDB's own hash join does.
///
/// What it cannot do is the kinds that have something to say about the gathered side. A `RIGHT` or a
/// `FULL` join keeps the gathered rows nothing matched, and which those are is not known until every
/// driving row has been through, so those stay on [`Join`] with its `finalize`. A `MARK` join asks a
/// question of the whole gathered side per driving row, and a `POSITIONAL` one is not a lookup at
/// all. [`streamed`] is the list.
#[derive(Debug)]
pub(crate) struct Probe {
    kind: JoinKind,
    equalities: Equalities,
    /// How wide a driving row is, which is how far to rotate a swapped one.
    left_width: usize,
    /// How wide a gathered row is, which is how much padding an unmatched driving row takes.
    right_width: usize,
    types: Vec<LogicalType>,
    schema: Schema,
    /// Whether this operator's left side is the plan's right one. See [`Join::swapped`].
    swapped: bool,
    /// The same token the `cancel` module wraps every node in, held here as well.
    ///
    /// The build is one call that reads a whole side, the same way the nested loop is, so the
    /// wrapper checking between chunks would not look at the token while it ran.
    cancel: Cancel,
    /// The gathered side, filled by the pipeline this one depends on.
    gathered: Rows,
    /// The rows and the table over them, built by whichever instance asks first.
    built: OnceLock<Result<Arc<Built>>>,
    /// What the table is charged, held for as long as it is readable.
    ///
    /// The rows themselves are not charged again here. They were charged as they were gathered and
    /// the gather holds that reservation until the pipeline that depends on it is done, so charging
    /// them a second time on the way into this operator would count one copy twice.
    held: Mutex<Reservation>,
}

/// The gathered side and the table that finds rows in it.
#[derive(Debug)]
struct Built {
    rows: Vec<Vec<Value>>,
    index: RowMap<Vec<usize>>,
}

/// Where one instance of a probe is in the driving chunk it was given.
#[derive(Debug)]
pub(crate) struct Probing {
    /// The driving chunk being walked, held while there is any of it left to answer.
    left: Option<Chunk>,
    row: usize,
    /// How many of the current row's matches have already come out.
    ///
    /// One driving row can match more rows than fit in a chunk, so a row is not always finished by
    /// the call that started it. Without this the join would either produce an oversized chunk or
    /// silently drop the rest of a popular key.
    hit: usize,
}

/// The kinds a lookup on its own answers.
///
/// Every one of these decides about a driving row from that row's matches alone, which is what makes
/// the answer a stream. The rest need something the whole pass knows, and they are on [`Join`].
pub(crate) fn streamed(kind: JoinKind) -> bool {
    matches!(
        kind,
        JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti | JoinKind::Single
    )
}

impl Probe {
    /// The probe for this join, or nothing when this is not a join a lookup answers.
    ///
    /// `left` is the schema of the side whose rows arrive here and `right` is the side the pipeline
    /// before this one gathered, with `kind` stated in those terms, all exactly as [`Join::new`]
    /// takes them.
    pub(crate) fn new(
        plan: &Plan,
        left: &Schema,
        right: &Gathered<'_>,
        kind: JoinKind,
        conditions: Slice,
        cancel: &Cancel,
        memory: &Memory,
    ) -> Option<Self> {
        if !streamed(kind) {
            return None;
        }
        let swapped = right.swapped;
        let right_schema = right.schema;
        let equalities = equalities(plan, plan.expr_list(conditions), left, right_schema)?;
        let schema = match kind {
            JoinKind::Semi | JoinKind::Anti => left.clone(),
            // The plan's order rather than this operator's, for the reason [`Join::new`] gives.
            _ if swapped => Schema::concat(right_schema, left),
            _ => Schema::concat(left, right_schema),
        };
        Some(Self {
            kind,
            equalities,
            left_width: left.width(),
            right_width: right_schema.width(),
            types: schema.types(),
            schema,
            swapped,
            cancel: cancel.clone(),
            gathered: right.rows.clone(),
            built: OnceLock::new(),
            held: Mutex::new(memory.reservation()),
        })
    }

    /// What this operator produces, which is both sides' columns unless the kind throws one away.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The gathered rows and the table over them, built once however many instances there are.
    ///
    /// Whichever instance asks first builds it and the others wait, which is what [`OnceLock`] does
    /// and is why the table is here rather than in the instance state. A failure is remembered the
    /// same way: the thing that fails is running out of memory building the table, and an instance
    /// that retried it would be retrying it against a budget that has not got any larger.
    fn built(&self) -> Result<Arc<Built>> {
        self.built
            .get_or_init(|| {
                let rows = self.gathered.take()?;
                let mut held = self.held.lock().map_err(poisoned)?;
                let index = index(&rows, &self.equalities.right, &self.cancel, &mut held)?;
                Ok(Arc::new(Built { rows, index }))
            })
            .clone()
    }
}

impl Stream for Probe {
    type Local = Probing;

    fn local(&self) -> Probing {
        Probing { left: None, row: 0, hit: 0 }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Probing) -> Result<Progress> {
        let built = self.built()?;
        let left = match local.left.take() {
            // Being asked again, so the chunk holds whatever was downstream of it and the driving
            // rows are the ones this instance kept.
            Some(left) => left,
            None => {
                local.row = 0;
                local.hit = 0;
                chunk.clone()
            }
        };
        let mut out: Vec<Vec<Value>> = Vec::new();
        while local.row < left.len() && out.len() < VECTOR_SIZE {
            // Once per driving row, the same granularity the nested loop checks at, and the only
            // place in this operator that runs long once the table is built.
            self.cancel.check()?;
            let row: Vec<Value> = left.row(local.row).collect();
            let found = hits(&built.index, &row, &self.equalities.left);
            match self.kind {
                JoinKind::Semi => {
                    if !found.is_empty() {
                        out.push(row);
                    }
                }
                JoinKind::Anti => {
                    if found.is_empty() {
                        out.push(row);
                    }
                }
                JoinKind::Single => {
                    if found.len() > 1 {
                        return Err(too_many_rows());
                    }
                    match found.first() {
                        Some(&hit) => out.push(pair(&row, &built.rows[hit])),
                        None => out.push(pad_right(&row, self.right_width)),
                    }
                }
                JoinKind::Left if found.is_empty() => {
                    out.push(pad_right(&row, self.right_width));
                }
                // An inner join with no match produces nothing, which is this arm with an empty
                // list, and the rest of it is one output row per match.
                _ => {
                    let room = VECTOR_SIZE - out.len();
                    let end = (local.hit + room).min(found.len());
                    for &hit in &found[local.hit..end] {
                        out.push(pair(&row, &built.rows[hit]));
                    }
                    if end < found.len() {
                        // A key with more matches than fit in a chunk. The row stays where it is and
                        // the next call picks up from the match this one stopped at.
                        local.hit = end;
                        break;
                    }
                    local.hit = 0;
                }
            }
            local.row += 1;
        }
        if self.swapped {
            // Back into the plan's order, for the reason [`Sink::finalize`] gives below. On the rows
            // rather than the chunk because the rows are owned here and the chunk is not built yet.
            for row in &mut out {
                row.rotate_left(self.left_width);
            }
        }
        *chunk = rows::pack(&self.types, &out)?;
        if local.row < left.len() {
            local.left = Some(left);
            return Ok(Progress::Again);
        }
        Ok(Progress::More)
    }
}

/// What a scalar subquery says when it turns out not to be scalar.
fn too_many_rows() -> Error {
    Error::invalid_input(
        "More than one row returned by a subquery used as an expression - scalar subqueries can only return a single row.\n\nUse \"SET scalar_subquery_error_on_multiple_rows=false\" to revert to previous behavior of returning a random row."
            .to_string(),
    )
}

/// The columns a join's equalities line up, by position on each side.
///
/// Two lists rather than a list of pairs because each of them is read whole: one builds the key of
/// a gathered row and the other builds the key of a driving row, and they are in the same order so
/// that the two keys are the same key.
#[derive(Debug)]
struct Equalities {
    /// Positions in a row of the driving side.
    left: Vec<usize>,
    /// Positions in a row of the gathered side, in the order the driving side's are in.
    right: Vec<usize>,
}

/// The columns a join's conditions line up, when every one of them lines two columns up.
///
/// This is the question that decides whether the nested loop runs at all. An equality between a
/// column of one side and a column of the other is answerable by looking the value up, and a
/// condition that is anything else is not, so a join whose conditions are all equalities is a join
/// that never has to compare a pair to find out whether it is a pair. It is also the question
/// `crates/rudb-exec/src/build.rs` asks to decide which of the two operators here to build, which is
/// why it is a function of the plan rather than a method on either of them.
///
/// All of them or none of them, for now. A join with an equality and something else could still use
/// the equality to find candidates and evaluate the rest over those, and that is the shape this
/// wants next. What it takes is building a chunk of the candidate rows to evaluate over, which is a
/// second copy of part of a side, and doing it before the plain case is measured would be adding the
/// complicated half first.
///
/// Only `=`. `IS NOT DISTINCT FROM` is the same lookup with the opposite null rule and is left out
/// because no plan in the suite writes one, and a rule with no query behind it is a rule nothing
/// checks.
fn equalities(
    plan: &Plan,
    conditions: &[ExprRef],
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<Equalities> {
    if conditions.is_empty() {
        return None;
    }
    let mut found = Equalities { left: Vec::new(), right: Vec::new() };
    for &condition in conditions {
        let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
            return None;
        };
        // The two sides of the equality have to be the same type, because what answers it is a hash
        // table and a hash table has one bucket for one value. The binder puts a cast in where the
        // types differ, and a cast is not a column, so this is a check rather than a conversion: an
        // equality that needed one has already failed the match below.
        if plan.expr_type(left) != plan.expr_type(right) || !looked_up(plan.expr_type(left)) {
            return None;
        }
        let (&Expr::Column(one), &Expr::Column(other)) = (plan.expr(left), plan.expr(right)) else {
            return None;
        };
        let across = match (
            left_schema.position_of(one).zip(right_schema.position_of(other)),
            left_schema.position_of(other).zip(right_schema.position_of(one)),
        ) {
            (Some(across), _) | (None, Some(across)) => across,
            // Both columns on one side, which is a predicate that should have been pushed into that
            // side and is not this operator's to be clever about.
            (None, None) => return None,
        };
        found.left.push(across.0);
        found.right.push(across.1);
    }
    Some(found)
}

/// The gathered side's rows, by the values the equalities read out of them.
///
/// A row whose key holds a null is left out rather than stored under a null key. `NULL = NULL` is
/// null and not true, so such a row matches nothing, and leaving it out is what says so. That is the
/// one place the key encoding here parts company with the one grouping uses, which answers
/// `IS NOT DISTINCT FROM` and puts every null in the same group.
///
/// The positions come out of one pass in order, so each entry's list is ascending and the rows a
/// probe finds arrive in the order the gathered side holds them. The nested loop produced them in
/// that order too, which is why this is a faster way to the same answer rather than the same answer
/// in a different order.
fn index(
    rows: &[Vec<Value>],
    at: &[usize],
    cancel: &Cancel,
    scratch: &mut Reservation,
) -> Result<RowMap<Vec<usize>>> {
    let mut index: RowMap<Vec<usize>> = RowMap::default();
    scratch.grow(rows::buckets(rows.len()))?;
    for (position, row) in rows.iter().enumerate() {
        let Some(key) = key(row, at) else { continue };
        // The key's own values and the position stored beside it. The list an entry holds grows by
        // one `usize` per row and the vector behind it doubles, so this charges the row it is about
        // rather than trying to say when a doubling happened.
        scratch.grow(rows::footprint(&key) + 8)?;
        index.entry(Key(key)).or_default().push(position);
        // Once per gathered row, which is the same granularity the probe checks at. A build over a
        // side nobody bounded is the one part of this operator that can run long without producing
        // anything.
        if position % 1024 == 0 {
            cancel.check()?;
        }
    }
    Ok(index)
}

/// The gathered rows one driving row matches, by position in the gathered side.
///
/// Nothing for a driving row whose key holds a null, which matches nothing for the same reason a
/// null on the other side was never stored. A row with no match is a row the join kind decides about
/// rather than one that is dropped here.
fn hits<'i>(index: &'i RowMap<Vec<usize>>, row: &[Value], at: &[usize]) -> &'i [usize] {
    match key(row, at) {
        None => &[],
        Some(key) => index.get(&Key(key)).map_or(&[], Vec::as_slice),
    }
}

/// The values at `at`, or nothing when one of them is null.
///
/// Nothing rather than a key holding a null, because the caller's two uses of that answer are the
/// same one: a null on either side of `=` makes the comparison null, so the row takes part in no
/// pair and there is nothing to look up or to store.
fn key(row: &[Value], at: &[usize]) -> Option<Vec<Value>> {
    let mut key = Vec::with_capacity(at.len());
    for &column in at {
        let value = &row[column];
        if matches!(value, Value::Null) {
            return None;
        }
        key.push(value.clone());
    }
    Some(key)
}

/// Whether two values of this type are equal exactly when they are the same key.
///
/// The scalar types are, which is what the hash table needs, and the nested ones are not asked.
/// A list containing a null compares to another list by SQL's rules and not by its bytes, so a
/// table that treated two such lists as one key would answer a join with rows `=` says nothing
/// about. There is no query behind allowing them and there is a wrong answer behind guessing.
fn looked_up(of: &LogicalType) -> bool {
    !matches!(
        of,
        LogicalType::List(_)
            | LogicalType::Array(_, _)
            | LogicalType::Struct(_)
            | LogicalType::Map(_, _)
            | LogicalType::Union(_)
            | LogicalType::Null
    )
}

/// Both rows, left then right.
fn pair(left: &[Value], right: &[Value]) -> Vec<Value> {
    let mut row = left.to_vec();
    row.extend(right.iter().cloned());
    row
}

/// A left row with nulls where the right side would be.
fn pad_right(left: &[Value], width: usize) -> Vec<Value> {
    let mut row = left.to_vec();
    row.extend(std::iter::repeat_n(Value::Null, width));
    row
}

/// A right row with nulls where the left side would be.
fn pad_left(width: usize, right: &[Value]) -> Vec<Value> {
    let mut row = vec![Value::Null; width];
    row.extend(right.iter().cloned());
    row
}

/// The nth left row beside the nth right row, padding the shorter side.
///
/// DuckDB's `POSITIONAL JOIN` does not stop at the shorter side, it fills the missing values with
/// nulls, so a positional join of three rows against five is five rows and not three.
fn positional(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    left_width: usize,
    right_width: usize,
) -> Vec<Vec<Value>> {
    let rows = left.len().max(right.len());
    (0..rows)
        .map(|at| match (left.get(at), right.get(at)) {
            (Some(left), Some(right)) => pair(left, right),
            (Some(left), None) => pad_right(left, right_width),
            (None, Some(right)) => pad_left(left_width, right),
            (None, None) => Vec::new(),
        })
        .collect()
}

/// One left row repeated across a chunk of the right side.
///
/// The left half is constant vectors, so pairing one left row with a thousand right ones costs one
/// value per left column rather than a thousand.
fn widen(left_row: &[Value], left_types: &[LogicalType], right: &Chunk) -> Result<Chunk> {
    let rows = right.len();
    let mut columns: Vec<Vector> = left_row
        .iter()
        .zip(left_types)
        .map(|(value, ty)| Vector::constant(ty.clone(), value.clone(), rows))
        .collect();
    columns.extend(right.columns().iter().cloned());
    Chunk::with_rows(columns, rows)
}

#[cfg(test)]
mod tests {
    use rudb_common::{Cancel, Field, LogicalType, Memory, Value};
    use rudb_plan::{ColumnBinding, JoinKind, Plan, Slice};
    use rudb_vector::{Data, Vector};

    use super::{Buffered, Chunk, CrossProduct, Gathered, Join, Progress, Schema, Sink, Stream};
    use crate::gather::{Gather, Keep, Rows};

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn schema(name: &str, table: u32) -> Schema {
        Schema::numbered(vec![Field::new(name, LogicalType::Integer)], table)
    }

    /// The right side of a join, run to the end the way the pipeline before this one would.
    fn gathered(memory: &Memory, values: &[i32]) -> (Gather, Rows) {
        let (gather, rows) = Gather::new(memory);
        let mut local = gather.local();
        if !values.is_empty() {
            gather.sink(&chunk(values), &mut local).expect("the right rows");
        }
        gather.combine(local).expect("the one instance");
        gather.finalize().expect("nothing to do");
        (gather, rows)
    }

    /// The one left chunk through the sink, and the answer out of the other end.
    fn run(join: &Join<'_>, left: &[i32]) {
        let mut local = join.local();
        if !left.is_empty() {
            join.sink(&chunk(left), &mut local).expect("the left rows");
        }
        join.combine(local).expect("the one instance");
        join.finalize().expect("the answer");
    }

    fn rows(out: &Buffered, width: usize) -> Vec<Vec<Value>> {
        let Some(chunk) = out.at(0).expect("readable") else { return Vec::new() };
        (0..chunk.len())
            .map(|row| (0..width).map(|column| chunk.value_at(row, column)).collect())
            .collect()
    }

    #[test]
    fn every_left_row_meets_every_right_row_when_there_is_no_condition() {
        let plan = Plan::new();
        let memory = Memory::unlimited();
        let (_gather, right) = gathered(&memory, &[10, 20]);
        let (join, out) = Join::new(
            &plan,
            &schema("a", 0),
            Gathered { schema: &schema("b", 1), rows: right, marker: None, swapped: false },
            JoinKind::Inner,
            Slice::EMPTY,
            &Cancel::new(),
            &memory,
        );

        run(&join, &[1, 2]);

        assert_eq!(
            rows(&out, 2),
            [
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    /// The same join built the other way round, which is what the builder does when the plan asks
    /// for the left input to be the gathered one.
    ///
    /// The operator's own left side is table 1 here, because table 1 is the side driving it, and
    /// the answer still comes out with table 0's column first, because that is what the plan said
    /// the join produces and everything above it was built against that.
    #[test]
    fn a_swapped_join_produces_the_plans_columns_in_the_plans_order() {
        let plan = Plan::new();
        let memory = Memory::unlimited();
        let (_gather, gathered_side) = gathered(&memory, &[1, 2]);
        let (join, out) = Join::new(
            &plan,
            &schema("b", 1),
            Gathered { schema: &schema("a", 0), rows: gathered_side, marker: None, swapped: true },
            JoinKind::Inner,
            Slice::EMPTY,
            &Cancel::new(),
            &memory,
        );

        run(&join, &[10, 20]);

        assert_eq!(join.schema().position_of(ColumnBinding::new(0, 0)), Some(0));
        assert_eq!(join.schema().position_of(ColumnBinding::new(1, 0)), Some(1));
        // Every pair the unswapped version of this produces, and only the order the rows arrive in
        // differs, because the nested loop walks the driving side outermost either way.
        assert_eq!(
            rows(&out, 2),
            [
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(20)],
            ]
        );
    }

    /// A `LEFT` join asked for with its inputs swapped is run as a `RIGHT` join, which is what the
    /// builder passes in. The padding has to land on the plan's right side rather than on the
    /// operator's, and this is the test that says which one that is.
    #[test]
    fn a_swapped_outer_join_pads_the_side_the_plan_called_the_right_one() {
        let plan = Plan::new();
        let memory = Memory::unlimited();
        let (_gather, gathered_side) = gathered(&memory, &[1, 2]);
        let (join, out) = Join::new(
            &plan,
            &schema("b", 1),
            Gathered { schema: &schema("a", 0), rows: gathered_side, marker: None, swapped: true },
            JoinKind::Right,
            Slice::EMPTY,
            &Cancel::new(),
            &memory,
        );

        // No driving rows at all, so every gathered row is unmatched and the mirrored kind is what
        // keeps them.
        run(&join, &[]);

        assert_eq!(
            rows(&out, 2),
            [vec![Value::Integer(1), Value::Null], vec![Value::Integer(2), Value::Null],]
        );
    }

    /// The kind that keeps the left row rather than pairing it, and the side of it that a right
    /// side with nothing in it is the easiest way to reach.
    #[test]
    fn an_anti_join_against_nothing_keeps_every_left_row() {
        let plan = Plan::new();
        let memory = Memory::unlimited();
        let (_gather, right) = gathered(&memory, &[]);
        let (join, out) = Join::new(
            &plan,
            &schema("a", 0),
            Gathered { schema: &schema("b", 1), rows: right, marker: None, swapped: false },
            JoinKind::Anti,
            Slice::EMPTY,
            &Cancel::new(),
            &memory,
        );

        run(&join, &[1, 2]);

        assert_eq!(rows(&out, 1), [vec![Value::Integer(1)], vec![Value::Integer(2)]]);
    }

    /// A positional join does not stop at the shorter side, which is DuckDB's rule and the one
    /// thing about this kind that is easy to get wrong.
    #[test]
    fn a_positional_join_pads_the_shorter_side() {
        let plan = Plan::new();
        let memory = Memory::unlimited();
        let (_gather, right) = gathered(&memory, &[10]);
        let (join, out) = Join::new(
            &plan,
            &schema("a", 0),
            Gathered { schema: &schema("b", 1), rows: right, marker: None, swapped: false },
            JoinKind::Positional,
            Slice::EMPTY,
            &Cancel::new(),
            &memory,
        );

        run(&join, &[1, 2]);

        assert_eq!(
            rows(&out, 2),
            [vec![Value::Integer(1), Value::Integer(10)], vec![Value::Integer(2), Value::Null],]
        );
    }

    /// Every output chunk of one cross product over one left chunk, in the order it produced them.
    fn crossed(cross: &CrossProduct, left: &[i32]) -> Vec<Vec<Value>> {
        let mut local = cross.local();
        let mut chunk = chunk(left);
        let mut out = Vec::new();
        loop {
            let progress = cross.push(&mut chunk, &mut local).expect("a chunk");
            out.extend(
                (0..chunk.len()).map(|row| vec![chunk.value_at(row, 0), chunk.value_at(row, 1)]),
            );
            if progress != Progress::Again {
                return out;
            }
            // What the driver hands back is whatever the operators below it left in the chunk, and
            // this is the cheapest stand in for that.
            chunk = Chunk::empty(&[]);
        }
    }

    /// The right side as two chunks, so that the walk over them is exercised rather than assumed.
    fn kept(memory: &Memory, first: &[i32], second: &[i32]) -> (Keep, Buffered) {
        let (keep, out) = Keep::new(memory);
        let mut local = keep.local();
        keep.sink(&chunk(first), &mut local).expect("the first right chunk");
        keep.sink(&chunk(second), &mut local).expect("the second");
        keep.combine(local).expect("the one instance");
        keep.finalize().expect("the chunks");
        (keep, out)
    }

    #[test]
    fn a_cross_product_pairs_one_left_row_with_one_right_chunk_at_a_time() {
        let memory = Memory::unlimited();
        let (_keep, right) = kept(&memory, &[10, 20], &[30]);
        let cross = CrossProduct::new(&schema("a", 0), &schema("b", 1), right);

        assert_eq!(
            crossed(&cross, &[1, 2]),
            [
                vec![Value::Integer(1), Value::Integer(10)],
                vec![Value::Integer(1), Value::Integer(20)],
                vec![Value::Integer(1), Value::Integer(30)],
                vec![Value::Integer(2), Value::Integer(10)],
                vec![Value::Integer(2), Value::Integer(20)],
                vec![Value::Integer(2), Value::Integer(30)],
            ]
        );
    }

    /// Nothing on the right is nothing at all, and the way a stream says that is an empty chunk it
    /// does not ask to be called again for.
    #[test]
    fn a_cross_product_with_nothing_on_the_right_produces_nothing() {
        let memory = Memory::unlimited();
        let (keep, right) = Keep::new(&memory);
        keep.combine(keep.local()).expect("an instance that saw nothing");
        keep.finalize().expect("no chunks");
        let cross = CrossProduct::new(&schema("a", 0), &schema("b", 1), right);

        let mut local = cross.local();
        let mut chunk = chunk(&[1, 2]);
        assert_eq!(cross.push(&mut chunk, &mut local).expect("no rows"), Progress::More);
        assert!(chunk.is_empty());
    }
}
