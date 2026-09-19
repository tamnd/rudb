//! Joins.
//!
//! Two ways of finding the rows one row matches, and one set of rules about what to do with them.
//!
//! A condition that is an equality with one side's columns on its left and the other side's on its
//! right is answered by looking the value up. The gathered side goes into a hash table once and each
//! driving row reads one entry out of it, so the work is the two sides added rather than multiplied.
//! That is the asymptotic half of `spec/07-execution.md` section 7.4 and it is where every join in a
//! query past the simplest one ends up.
//!
//! What goes into the table is the value of an expression rather than the contents of a column. The
//! binder writes `p.k::INTEGER = b.k` as a cast around one operand, and a join key that had to be a
//! bare column would look at that cast, fail to recognise it, and hand the whole join to the nested
//! loop. Joining a million rows to a hundred thousand that way is ten to the eleventh pairs, which
//! is hours, so the difference between reading a column and evaluating an expression over it is the
//! difference between a query that answers and a query that does not. The keys are evaluated a chunk
//! at a time on both sides, through the same vectorized evaluator the nested loop uses, so an
//! expression key costs one pass over a side and not one call per row.
//!
//! One equality is enough. `ON p.k = b.k AND p.g >= b.g` is a lookup on the equality and then the
//! inequality evaluated on the few candidates it found, and a join that fell to the loop for the
//! company its equality kept was doing every pair of both sides to answer a handful. That is sound
//! because an equality filters pairs and so does everything beside it: narrowing first and filtering
//! after is the same set of pairs reached in a cheaper order.
//!
//! A condition with no equality in it at all is a nested loop: it is evaluated over the driving row
//! paired with a whole chunk of the gathered side, which keeps the evaluator on its batch interface
//! and makes the driving row's columns constant vectors that cost one value each. It is the only
//! answer for a join written entirely on ranges, and it is what the equality path is read against,
//! since eight join kinds each have their own rule about a row with no match and the way to find out
//! whether the faster path has them right is to run both and diff.
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
//! The null rule is per column and not per table. A group key answers `IS NOT DISTINCT FROM`, where
//! two nulls are one key, and `=` answers null for a null on either side, so a row whose key holds a
//! null takes part in no pair. Both comparisons turn up here, because a query writes `=` and the
//! unnesting rules write `IS NOT DISTINCT FROM` to equate a domain key, and one join can hold both.
//! The key encoding is the one grouping uses either way, so what the two differ in is only whether a
//! row with a null in that column goes into the table and the probe at all.
//!
//! # Two pipelines and an edge
//!
//! A join is two inputs, and two inputs is two pipelines with a dependency between them. One side
//! ends in a [`Keep`](crate::gather::Keep), which holds on to its chunks as the chunks they are and
//! does nothing else, and the other ends here. The order is not a choice: no row of the second side can be answered until every
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
use rudb_pipeline::{Lease, Progress, Sink, Stream};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Plan, Slice};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::buffer::Buffered;
use crate::expr::evaluate_all_in_time_zone;
use crate::gather::{self, Gathering};
use crate::lookup::{Lookup, MISS, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::side::{Build, PAD};

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
    right: Buffered,
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
    /// The chunks, readable once the pipeline that filled them has finished.
    ///
    /// Chunks rather than rows. This side is going to be read by position, once to build the table
    /// and then once per match, and taking it apart into a `Vec<Value>` per row on the way in would
    /// be an allocation per row for a layout that then has to be transposed back into columns
    /// anyway. See [`Build`], which is where they are laid end to end.
    pub(crate) chunks: Buffered,
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
            right: right.chunks,
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
    fn joined(&self, left_rows: &[Vec<Value>], right_chunks: &[Chunk]) -> Result<Vec<Vec<Value>>> {
        // The rows being paired up, charged apart from the chunks that come out, because a nested
        // loop join holds all of it at once and gives back everything but the output when it is
        // done.
        let mut scratch = self.memory.reservation();
        let left_types = self.left_schema.types();
        let right_types = self.right_schema.types();
        // The gathered side as columns, which is what a match is read out of. The kinds on this
        // operator still pair rows up one at a time, so what they take out of it is a row, but the
        // side itself is held the way the stream next door holds it and the residual reads it by
        // gathering. See [`Build`] and #880 for the rest of the way.
        let right = Build::new(&right_types, right_chunks)?;
        scratch.grow(right.footprint())?;
        let right_rows = right.rows();
        if self.kind == JoinKind::Positional {
            let rows: Vec<Vec<Value>> = (0..right_rows).map(|at| right.row(at as u32)).collect();
            return Ok(positional(left_rows, &rows, left_types.len(), right_types.len()));
        }
        // A mark join asks a question about the whole of the gathered side rather than collecting
        // the rows that match, so the rest of this function, which is written around a list of
        // matches, has nothing it can do for it. It gets its answer above the loop where a lookup
        // gives one and stays on the loop where none does.
        let marks = match self.kind {
            JoinKind::Mark => self.marks(left_rows, right_chunks, &mut scratch)?,
            _ => None,
        };
        let equalities = if self.kind == JoinKind::Mark {
            None
        } else {
            equalities(self.plan, &self.conditions, &self.left_schema, &self.right_schema)
        };
        let index = match &equalities {
            Some(equalities) => Some(lookup(
                equalities.gathered(self.plan, &self.right_schema, self.time_zone),
                right_chunks,
                &self.cancel,
                &mut scratch,
            )?),
            None => None,
        };
        // The driving side looked up up front rather than one row at a time inside the loop,
        // because the lookup works on a chunk and the loop below works on a row. What it costs is
        // one `usize` per driving row held while the join runs, which is the price of keeping the
        // loop's shape, and it is charged. The stream next door does not pay even that: it has the
        // driving chunk in hand and looks it up as it arrives.
        let left_slots = match (&equalities, &index) {
            (Some(equalities), Some(index)) => found(
                equalities.driving(self.plan, &self.left_schema, self.time_zone),
                index,
                left_rows,
                &self.cancel,
                &mut scratch,
            )?,
            _ => Vec::new(),
        };
        let residual = equalities.as_ref().map(|equalities| Residual {
            plan: self.plan,
            exprs: &equalities.residual,
            combined: &self.combined,
            left_types: &left_types,
            time_zone: self.time_zone,
        });
        // Nothing evaluates a condition over the whole gathered side when the index narrows it
        // first. What the residual evaluates over is a chunk of the candidates, gathered one
        // driving row at a time.
        let scanned_over: &[Chunk] = match index {
            Some(_) => &[],
            None => right_chunks,
        };
        let mut matched = vec![false; right_rows];
        scratch.grow(u64::try_from(right_rows).unwrap_or(u64::MAX))?;
        let mut out: Vec<Vec<Value>> = Vec::new();
        // Reused across driving rows, so that a join with a residual allocates once rather than
        // once per row it looks up.
        let mut kept: Vec<u32> = Vec::new();
        // The same, for the chain the lookup answers a driving row with.
        let mut chain: Vec<u32> = Vec::new();
        for (position, left_row) in left_rows.iter().enumerate() {
            // Once per left row, in the same place and for the same reason as the reservation at
            // the bottom of the loop. What a query can run past its clock by is one pass over the
            // right side, which is the smallest unit this loop has that is not the inner one.
            self.cancel.check()?;
            let before = out.len();
            if self.kind == JoinKind::Mark {
                let marker = match &marks {
                    Some(marks) => marks[position].clone(),
                    None => self.marker(left_row, &left_types, scanned_over)?,
                };
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
            let hits: &[u32] = match (&index, &residual) {
                (Some(index), Some(residual)) => {
                    index.matches(left_slots[position], &mut chain);
                    residual.keep(left_row, &right, &chain, &mut kept)?
                }
                _ => {
                    scanned = self.matching(left_row, &left_types, scanned_over)?;
                    &scanned
                }
            };
            for &hit in hits {
                matched[hit as usize] = true;
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
                        Some(&hit) => out.push(pair(left_row, &right.row(hit))),
                        None => out.push(pad_right(left_row, right_types.len())),
                    }
                }
                JoinKind::Left | JoinKind::Full if hits.is_empty() => {
                    out.push(pad_right(left_row, right_types.len()));
                }
                _ => {
                    for &hit in hits {
                        out.push(pair(left_row, &right.row(hit)));
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
                    out.push(pad_left(left_types.len(), &right.row(at as u32)));
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
    ) -> Result<Vec<u32>> {
        let mut hits = Vec::new();
        let mut base: u32 = 0;
        for chunk in right_chunks {
            let rows = u32::try_from(chunk.len()).unwrap_or(PAD);
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
                    if is_true(&merged.value_at(row as usize)) {
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

    /// Every driving row's marker, answered by lookup, or `None` where no lookup answers it.
    ///
    /// A mark join's answer is three valued and it is about the whole gathered side: true where
    /// some row of it satisfies the condition, false where no row does, null where no row does but
    /// some row could not be decided. [`Join::marker`] gets that by evaluating the condition over
    /// every pair of the two sides, which is the two sides multiplied. TPC-H q18 asks whether each
    /// of a million and a half orders is one of the fifty seven the subquery returned, so that is
    /// eighty five million evaluations to answer a question one pass over each side answers.
    ///
    /// The rule below is exact for one equality and is not exact for more than one, so one is what
    /// this takes. Read the condition as `d = g` over a gathered side with rows in it. A hit means
    /// some `g` equalled `d`, so the answer is true. A miss means no `g` equalled `d`, so the
    /// answer is false unless some pair came out null rather than false, and `d = g` is null
    /// exactly when one of its operands is: when `d` is null, or when the gathered side holds a
    /// null. Neither of those is a question about which row missed, so both are settled outside the
    /// lookup. A gathered side with no rows in it has no pairs at all, so every marker is false.
    ///
    /// Two equalities are handed back to the loop, because `d1 = g1 AND d2 = g2` is false whenever
    /// either half is false whatever the other half is. A miss is null there only where some
    /// gathered row agreed on every column it had a value for, which is a question about rows and
    /// not about the side, and answering it as though it were the one above would call a false
    /// marker null. Every mark join in TPC-H is one equality on one column.
    ///
    /// A residual is handed back for the same reason: the rows the lookup found still have to be
    /// filtered, and the rows it did not find are the ones the null rule is about.
    ///
    /// `IS NOT DISTINCT FROM` needs nothing extra. It is never null, and the table holds nulls as
    /// values, so the marker is whether the lookup hit and the null rule below never fires.
    fn marks(
        &self,
        left_rows: &[Vec<Value>],
        right_chunks: &[Chunk],
        scratch: &mut Reservation,
    ) -> Result<Option<Vec<Value>>> {
        let Some(found) =
            equalities(self.plan, &self.conditions, &self.left_schema, &self.right_schema)
        else {
            return Ok(None);
        };
        if found.left.len() != 1 || !found.residual.is_empty() {
            return Ok(None);
        }
        if right_chunks.iter().all(Chunk::is_empty) {
            return Ok(Some(vec![Value::Boolean(false); left_rows.len()]));
        }
        let nulls_are_values = found.null_is_a_value[0];
        let gathered = found.gathered(self.plan, &self.right_schema, self.time_zone);
        // One pass over the gathered side, asked once here rather than once per driving row,
        // because what it decides is the same for all of them.
        let undecided = !nulls_are_values && any_null_key(gathered, right_chunks, &self.cancel)?;
        let index = lookup(gathered, right_chunks, &self.cancel, scratch)?;
        let types = self.left_schema.types();
        scratch.grow(u64::try_from(left_rows.len()).unwrap_or(u64::MAX))?;
        let mut marks = Vec::with_capacity(left_rows.len());
        let mut probing = Scratch::default();
        let mut slots = Vec::new();
        for batch in left_rows.chunks(VECTOR_SIZE) {
            // Once per batch, for the same reason the build and the probe below check there. A side
            // nobody bounded is what makes this run long and it produces nothing until it is done.
            self.cancel.check()?;
            let chunk = rows::pack(&types, batch)?;
            let columns = evaluate_all_in_time_zone(
                self.plan,
                &found.left,
                &self.left_schema,
                &chunk,
                self.time_zone,
            )?;
            index.slots(&columns, batch.len(), &found.null_is_a_value, &mut probing, &mut slots);
            // row at a time: which of the three answers a driving row gets depends on that row's
            // own slot and its own key, and a slot is not something the boolean kernel reduces.
            for (row, &slot) in slots.iter().take(batch.len()).enumerate() {
                marks.push(if slot != MISS {
                    Value::Boolean(true)
                } else if undecided || (!nulls_are_values && columns[0].is_null_at(row)) {
                    Value::Null
                } else {
                    Value::Boolean(false)
                });
            }
        }
        Ok(Some(marks))
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

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let left_rows = std::mem::take(&mut *self.left.lock().map_err(poisoned)?);
        let right_chunks = held(&self.right)?;
        let mut out = self.joined(&left_rows, &right_chunks)?;
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
        drop(right_chunks);
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
pub(crate) struct Probe<'a> {
    plan: &'a Plan,
    kind: JoinKind,
    equalities: Equalities,
    /// What a driving row looks like, which is what the driving key expressions resolve against.
    left_schema: Schema,
    /// What a gathered row looks like, which is what the gathered key expressions resolve against.
    right_schema: Schema,
    /// Both sides' columns in this operator's order, which is what the residual resolves against.
    ///
    /// This operator's order and not the plan's even when swapped, for the reason [`Join::swapped`]
    /// gives: a condition finds its columns by binding rather than by counting.
    combined: Schema,
    left_types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    /// How many columns of the answer are the driving side's, which is how far to rotate a swapped
    /// one.
    left_width: usize,
    schema: Schema,
    /// Whether this operator's left side is the plan's right one. See [`Join::swapped`].
    swapped: bool,
    /// The same token the `cancel` module wraps every node in, held here as well.
    ///
    /// The build is one call that reads a whole side, the same way the nested loop is, so the
    /// wrapper checking between chunks would not look at the token while it ran.
    cancel: Cancel,
    /// The gathered side, filled by the pipeline this one depends on.
    gathered: Buffered,
    /// The rows and the table over them, built by whichever instance asks first.
    built: OnceLock<Result<Arc<Built>>>,
    /// What the table is charged, held for as long as it is readable.
    ///
    /// The rows themselves are not charged again here. They were charged as they were gathered and
    /// the gather holds that reservation until the pipeline that depends on it is done, so charging
    /// them a second time on the way into this operator would count one copy twice.
    held: Mutex<Reservation>,
    /// The parsed zone used by casts in the key expressions.
    time_zone: SessionTimeZone,
}

/// The gathered side and the table that finds rows in it.
#[derive(Debug)]
struct Built {
    rows: Build,
    index: Lookup,
}

/// Where one instance of a probe is in the driving chunk it was given.
#[derive(Debug)]
pub(crate) struct Probing {
    /// The driving chunk being walked, held while there is any of it left to answer.
    left: Option<Chunk>,
    /// That chunk's key columns, evaluated once when the chunk arrived.
    ///
    /// Empty when there is nothing to look up, which is a gathered side with no keyed rows in it.
    /// Every lookup misses then, so evaluating the driving side's key expressions would be work
    /// thrown away, and a key expression that raises on a row nothing could have matched would be
    /// raising where the nested loop this replaces never evaluated anything at all.
    keys: Vec<Vector>,
    /// That chunk's slot per driving row, looked up once when the chunk arrived.
    ///
    /// Once per chunk rather than once per row because the probe is a batch at a time. See
    /// [`Lookup::slots`], which is the whole argument.
    slots: Vec<usize>,
    /// The buffers that lookup walks the chunk with, held here so that a chunk costs no allocation.
    scratch: Scratch,
    /// The gathered rows the current driving row matches, refilled per row from its chain.
    chain: Vec<u32>,
    /// The ones of those a residual condition kept, when there is a residual condition.
    kept: Vec<u32>,
    /// Which driving row each output row reads from, one entry per output row.
    ///
    /// This and the one below it are the answer. A pair is two numbers, so the row loop writes two
    /// numbers, and the columns are gathered at those positions once the loop is done. See
    /// [`Build`], which is the whole argument.
    left_at: Vec<u32>,
    /// Which gathered row each output row reads from, [`PAD`] for a row that matched nothing.
    right_at: Vec<u32>,
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

impl<'a> Probe<'a> {
    /// The probe for this join, or nothing when this is not a join a lookup answers.
    ///
    /// `left` is the schema of the side whose rows arrive here and `right` is the side the pipeline
    /// before this one gathered, with `kind` stated in those terms, all exactly as [`Join::new`]
    /// takes them.
    pub(crate) fn new(
        plan: &'a Plan,
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
            plan,
            kind,
            equalities,
            left_schema: left.clone(),
            right_schema: right_schema.clone(),
            combined: Schema::concat(left, right_schema),
            left_types: left.types(),
            right_types: right_schema.types(),
            left_width: left.width(),
            schema,
            swapped,
            cancel: cancel.clone(),
            gathered: right.chunks.clone(),
            built: OnceLock::new(),
            held: Mutex::new(memory.reservation()),
            time_zone: SessionTimeZone::default(),
        })
    }

    /// Applies the session semantics to the key expressions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.time_zone = session.session_time_zone();
        self
    }

    /// What this operator produces, which is both sides' columns unless the kind throws one away.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The key this join can hand to the scan under its driving side, if there is one.
    ///
    /// The gathered side's key expression, which is what the range is measured over, and the driving
    /// column it is compared against, which is what a scan can be told about.
    /// [`Sideways`](crate::sideways::Sideways) has the argument for each of the three refusals here:
    /// the kind has to be one that drops a driving row with no match, the equality has to be the
    /// rule under which a null key matches nothing, and the driving half has to be a column rather
    /// than an expression, because a range about a value says nothing about what an expression over
    /// it produces.
    pub(crate) fn sideways(&self) -> Option<(ExprRef, ColumnBinding)> {
        if !matches!(self.kind, JoinKind::Inner | JoinKind::Semi) {
            return None;
        }
        let at = self.equalities.null_is_a_value.iter().position(|&stored| !stored)?;
        let Expr::Column(binding) = *self.plan.expr(*self.equalities.left.get(at)?) else {
            return None;
        };
        Some((*self.equalities.right.get(at)?, binding))
    }

    /// The conjuncts the lookup did not answer, over a pair of this join's two sides.
    fn residual(&self) -> Residual<'_> {
        Residual {
            plan: self.plan,
            exprs: &self.equalities.residual,
            combined: &self.combined,
            left_types: &self.left_types,
            time_zone: self.time_zone,
        }
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
                let chunks = held(&self.gathered)?;
                let mut charged = self.held.lock().map_err(poisoned)?;
                let index = lookup(
                    self.equalities.gathered(self.plan, &self.right_schema, self.time_zone),
                    &chunks,
                    &self.cancel,
                    &mut charged,
                )?;
                // The chunks laid end to end, which is a copy of the side and is charged as one.
                // The chunks themselves are not charged again here: the keep that made them holds
                // that reservation for as long as this operator can read them, and charging the
                // same bytes twice would be a limit half the size it says it is.
                let rows = Build::new(&self.right_types, &chunks)?;
                charged.grow(rows.footprint())?;
                Ok(Arc::new(Built { rows, index }))
            })
            .clone()
    }
}

impl Stream for Probe<'_> {
    type Local = Probing;

    fn local(&self) -> Probing {
        Probing {
            left: None,
            keys: Vec::new(),
            slots: Vec::new(),
            scratch: Scratch::default(),
            chain: Vec::new(),
            kept: Vec::new(),
            left_at: Vec::new(),
            right_at: Vec::new(),
            row: 0,
            hit: 0,
        }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Probing) -> Result<Progress> {
        let built = self.built()?;
        let left = match local.left.take() {
            // Being asked again, so the chunk holds whatever was downstream of it and the driving
            // rows are the ones this instance kept. Their keys are kept beside them, because
            // evaluating them again would be the same answer at the same price.
            Some(left) => left,
            None => {
                local.row = 0;
                local.hit = 0;
                let left = chunk.clone();
                // Once per driving chunk rather than once per driving row, which is what keeps the
                // evaluator on its batch interface here as well. Nothing at all against an empty
                // table, for the reason [`Probing::keys`] gives.
                local.keys = if built.index.is_empty() {
                    Vec::new()
                } else {
                    evaluate_all_in_time_zone(
                        self.plan,
                        &self.equalities.left,
                        &self.left_schema,
                        &left,
                        self.time_zone,
                    )?
                };
                // The lookup for the whole chunk at once, which is the hash in one pass per key
                // column and the probe a batch of rows at a time. Doing it here rather than in the
                // row loop below is what keeps the driving side on its batch interface.
                local.slots.clear();
                if !local.keys.is_empty() {
                    built.index.slots(
                        &local.keys,
                        left.len(),
                        &self.equalities.null_is_a_value,
                        &mut local.scratch,
                        &mut local.slots,
                    );
                }
                left
            }
        };
        let residual = self.residual();
        // The two halves of the answer, one entry per output row. Nothing is built here but a pair
        // of numbers per pair of rows, and the columns are gathered at those numbers below.
        local.left_at.clear();
        local.right_at.clear();
        while local.row < left.len() && local.left_at.len() < VECTOR_SIZE {
            // Once per driving row, the same granularity the nested loop checks at, and the only
            // place in this operator that runs long once the table is built.
            self.cancel.check()?;
            // The chain the lookup above left for this row, which is one walk of a run of `u32`
            // rather than a hash and a map lookup.
            let slot = local.slots.get(local.row).copied().unwrap_or(MISS);
            built.index.matches(slot, &mut local.chain);
            // The driving row as values only when there is a conjunct left to evaluate on it, which
            // is what makes an ordinary equi join cost no boxed row at all. A residual still gets
            // one per driving row, because the pairs it evaluates over are one row repeated across
            // its candidates and a constant vector is built out of a value.
            let found: &[u32] = if residual.exprs.is_empty() {
                &local.chain
            } else {
                let values: Vec<Value> = left.row(local.row).collect();
                residual.keep(&values, &built.rows, &local.chain, &mut local.kept)?
            };
            let at = u32::try_from(local.row).map_err(|_| too_many_rows())?;
            match self.kind {
                JoinKind::Semi => {
                    if !found.is_empty() {
                        local.left_at.push(at);
                    }
                }
                JoinKind::Anti => {
                    if found.is_empty() {
                        local.left_at.push(at);
                    }
                }
                JoinKind::Single => {
                    if found.len() > 1 {
                        return Err(too_many_rows());
                    }
                    local.left_at.push(at);
                    local.right_at.push(found.first().copied().unwrap_or(PAD));
                }
                JoinKind::Left if found.is_empty() => {
                    local.left_at.push(at);
                    local.right_at.push(PAD);
                }
                // An inner join with no match produces nothing, which is this arm with an empty
                // list, and the rest of it is one output row per match.
                _ => {
                    let room = VECTOR_SIZE - local.left_at.len();
                    let end = (local.hit + room).min(found.len());
                    for &hit in &found[local.hit..end] {
                        local.left_at.push(at);
                        local.right_at.push(hit);
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
        let mut columns: Vec<Vector> = left
            .columns()
            .iter()
            .map(|column| column.gather(&local.left_at))
            .collect::<Result<Vec<_>>>()?;
        // A semi or an anti join answers with the driving row alone, so there is no gathered half
        // to put beside it and no positions were written for one.
        if !matches!(self.kind, JoinKind::Semi | JoinKind::Anti) {
            columns.extend(built.rows.gather(&local.right_at)?);
        }
        if self.swapped {
            // Back into the plan's order, for the reason [`Sink::finalize`] gives below. One
            // rotation of the column list rather than a rotation per row, which is what holding the
            // answer as columns buys here as well.
            columns.rotate_left(self.left_width);
        }
        *chunk = Chunk::with_rows(columns, local.left_at.len())?;
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

/// The expressions a join's equalities line up, one pair per equality.
///
/// Two lists rather than a list of pairs because each of them is read whole: one builds the key of
/// a gathered row and the other builds the key of a driving row, and they are in the same order so
/// that the two keys are the same key.
#[derive(Debug)]
struct Equalities {
    /// Key expressions over a row of the driving side.
    left: Vec<ExprRef>,
    /// Key expressions over a row of the gathered side, in the order the driving side's are in.
    right: Vec<ExprRef>,
    /// Whether a null in this column is a value to match on, one entry per column above.
    ///
    /// True where the condition was `IS NOT DISTINCT FROM`, which two nulls answer true, and false
    /// where it was `=`, which they answer null. It is per column rather than per join because one
    /// join can be written both ways, and the unnesting rules write exactly that: the domain key is
    /// equated with `IS NOT DISTINCT FROM` so that an outer row whose key is null finds its own
    /// answer, while the condition the query wrote next to it is still an ordinary `=`.
    null_is_a_value: Vec<bool>,
    /// The conjuncts no lookup answers, to be evaluated on the pairs the lookup found.
    ///
    /// Usually empty. When it is not, the join is something like `ON p.k = b.k AND p.g >= b.g`,
    /// where the equality finds a handful of candidates per driving row and the inequality throws
    /// some of them away. Doing it the other way round, which is what a join with anything unkeyed
    /// in it used to do, is every pair of both sides.
    residual: Vec<ExprRef>,
}

impl Equalities {
    /// What it takes to read the driving side's keys out of its rows.
    fn driving<'a>(
        &'a self,
        plan: &'a Plan,
        schema: &'a Schema,
        time_zone: SessionTimeZone,
    ) -> Keying<'a> {
        Keying { plan, exprs: &self.left, schema, nulls: &self.null_is_a_value, time_zone }
    }

    /// The same for the gathered side, which is the half that goes into the table.
    fn gathered<'a>(
        &'a self,
        plan: &'a Plan,
        schema: &'a Schema,
        time_zone: SessionTimeZone,
    ) -> Keying<'a> {
        Keying { plan, exprs: &self.right, schema, nulls: &self.null_is_a_value, time_zone }
    }
}

/// One side's half of the equalities, with everything it takes to evaluate them.
///
/// The two halves are the same shape and are read by the same code, and a function that took the
/// five of them apart would be a function whose arguments could be given in the wrong order. Built
/// by [`Equalities::driving`] and [`Equalities::gathered`], which is where the choice of half is
/// made and the only place it can be made wrongly.
#[derive(Debug, Clone, Copy)]
struct Keying<'a> {
    plan: &'a Plan,
    /// The key expressions over a row of this side, one per equality.
    exprs: &'a [ExprRef],
    /// What a row of this side looks like, which is what those expressions resolve against.
    schema: &'a Schema,
    /// Whether a null in each key column is a value to match on. See [`Equalities::null_is_a_value`].
    nulls: &'a [bool],
    /// The parsed zone the casts in those expressions read.
    time_zone: SessionTimeZone,
}

/// The conjuncts the lookup did not answer, and what it takes to evaluate them on a pair.
///
/// The same work the nested loop does, over the candidates a lookup found rather than over both
/// sides. `ON p.k = b.k AND p.g >= b.g` against two million driving rows and six hundred thousand
/// gathered ones is more than a million million pairs on the loop, and one lookup plus a handful of
/// comparisons per driving row here.
#[derive(Debug, Clone, Copy)]
struct Residual<'a> {
    plan: &'a Plan,
    /// What is left of the condition, all of which has to hold. See [`Equalities::residual`].
    exprs: &'a [ExprRef],
    /// Both sides' columns in this operator's order, which is what those conjuncts resolve against.
    combined: &'a Schema,
    /// What a driving row looks like, for repeating one across the candidates.
    left_types: &'a [LogicalType],
    time_zone: SessionTimeZone,
}

impl Residual<'_> {
    /// The candidates in `hits` this keeps, which is all of them when there is nothing left to say.
    ///
    /// `into` is a buffer the caller owns so that a join does not allocate once per driving row, and
    /// it is not read when the answer is `hits` itself.
    ///
    /// The pairs are built a vector at a time and evaluated the way the nested loop evaluates them,
    /// through one chunk of the candidates with the driving row repeated across it as constants. A
    /// key with more candidates than fit in a vector is several passes rather than one oversized
    /// chunk, which is the same rule the rest of this file follows. The chunk of candidates is one
    /// gather per column out of the gathered side, which is the same kernel the answer is built
    /// with and the reason this does not box a row either.
    ///
    /// One difference from the loop this replaces, and it is the right way round: a conjunct that
    /// raises is now only evaluated on pairs the equality already accepted, so a join that used to
    /// fail on some pair the equality would have thrown away answers instead. DuckDB is the same,
    /// because a condition on a pair that no pair reaches is a condition about nothing.
    fn keep<'h>(
        &self,
        left_row: &[Value],
        rows: &Build,
        hits: &'h [u32],
        into: &'h mut Vec<u32>,
    ) -> Result<&'h [u32]> {
        if self.exprs.is_empty() || hits.is_empty() {
            return Ok(hits);
        }
        into.clear();
        for batch in hits.chunks(VECTOR_SIZE) {
            let chunk = rows.chunk(batch)?;
            let combined = widen(left_row, self.left_types, &chunk)?;
            let flags = evaluate_all_in_time_zone(
                self.plan,
                self.exprs,
                self.combined,
                &combined,
                self.time_zone,
            )?;
            let merged = combine(Connective::And, &flags)?;
            // row at a time: the flags are already a vector here, so what this wants is the
            // selection that 2c (#57) threads.
            for (at, &hit) in batch.iter().enumerate() {
                if is_true(&merged.value_at(at)) {
                    into.push(hit);
                }
            }
        }
        Ok(into)
    }
}

/// Which side of a join an expression reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Driving,
    Gathered,
}

/// The side every column of `expr` comes from, or nothing when it is not exactly one side.
///
/// Both sides is nothing, because an expression over both is a comparison in disguise and there is
/// no value to put in a table for it. Neither side is nothing too: an expression over no columns at
/// all is a constant, and `1 = 1` beside a real equality is a filter the optimizer should have
/// folded rather than a key that lines two sides up. A column in neither schema is a plan this
/// operator cannot resolve, and guessing a side for it is how a join answers with the wrong rows.
fn side_of(plan: &Plan, expr: ExprRef, driving: &Schema, gathered: &Schema) -> Option<Side> {
    let mut side = None;
    let mut mixed = false;
    columns(plan, expr, &mut |binding| {
        let found = if driving.position_of(binding).is_some() {
            Some(Side::Driving)
        } else if gathered.position_of(binding).is_some() {
            Some(Side::Gathered)
        } else {
            None
        };
        match (side, found) {
            (_, None) => mixed = true,
            (None, Some(one)) => side = Some(one),
            (Some(held), Some(one)) => mixed |= held != one,
        }
    });
    if mixed { None } else { side }
}

/// Calls `found` for every column `expr` reads.
///
/// The same walk `rudb_opt`'s `walk::columns` does, written again here because that one is private
/// to the optimizer and this crate does not depend on it. Total over [`Expr`] on purpose rather than
/// with a catch-all arm: a variant added later that holds expressions has to be added here too, and
/// a match that compiles while quietly missing one would make a join key out of an expression whose
/// columns nobody looked at.
fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    match *plan.expr(expr) {
        Expr::Column(binding) => found(binding),
        Expr::Constant(_) => {}
        Expr::Cast { input, .. } => columns(plan, input, found),
        Expr::Compare { left, right, .. } => {
            columns(plan, left, found);
            columns(plan, right, found);
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            for &child in plan.expr_list(children) {
                columns(plan, child, found);
            }
        }
        Expr::Aggregate { args, filter, .. } | Expr::Window { args, filter, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
        }
        Expr::Case { arms, otherwise } => {
            for arm in plan.arm_list(arms) {
                columns(plan, arm.when, found);
                columns(plan, arm.then, found);
            }
            if let Some(inner) = otherwise {
                columns(plan, inner, found);
            }
        }
    }
}

/// The expressions a join's conditions line up, when any of them line two sides up.
///
/// This is the question that decides whether the nested loop runs at all. An equality whose two
/// operands read opposite sides is answerable by looking the value up, so a join with one of those
/// in it never has to compare a pair to find out whether it is a candidate. It is also the question
/// `crates/rudb-exec/src/build.rs` asks to decide which of the two operators here to build, which is
/// why it is a function of the plan rather than a method on either of them.
///
/// An operand is an expression and not a column. `ON p.k::INTEGER = b.k` binds to a cast around one
/// operand and `ON upper(a.name) = b.name` to a call, and both of those are a value per row that a
/// table can be keyed on exactly as a column is. What matters is not the shape of the operand but
/// where its columns come from, which is what [`side_of`] answers.
///
/// One equality is enough. Everything else the condition says goes into [`Equalities::residual`] and
/// is evaluated on the pairs the lookup found, which is the difference between `ON p.k = b.k AND
/// p.g >= b.g` costing a lookup per driving row and costing every pair of both sides. What makes
/// that sound is that an equality is a filter on pairs and so is everything beside it, so narrowing
/// first and then filtering is the same set of pairs in a different order. Nothing at all is still
/// nothing: a join with no equality in it has no candidates to narrow to and stays on the loop.
///
/// `=` and `IS NOT DISTINCT FROM`, which are the same lookup with opposite null rules. The table
/// already holds a row under a key that may contain a null, because [`crate::key::Key`] compares
/// column by column the way grouping does, so what the two spellings differ in is whether a null
/// goes into the table at all. That is one flag per column and not a second kind of table.
fn equalities(
    plan: &Plan,
    conditions: &[ExprRef],
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<Equalities> {
    let mut found = Equalities {
        left: Vec::new(),
        right: Vec::new(),
        null_is_a_value: Vec::new(),
        residual: Vec::new(),
    };
    for &condition in conditions {
        let Expr::Compare { op: op @ (CompareOp::Equal | CompareOp::NotDistinctFrom), left, right } =
            *plan.expr(condition)
        else {
            found.residual.push(condition);
            continue;
        };
        // The two operands have to agree on a type, because what answers the equality is a hash
        // table and a hash table has one bucket for one value. Where they did not agree the binder
        // has already put a cast in, and that cast is part of the key expression rather than
        // something that stops the lookup, so this is the check that the binder did its half.
        if plan.expr_type(left) != plan.expr_type(right) || !plan.expr_type(left).is_keyed() {
            found.residual.push(condition);
            continue;
        }
        match (
            side_of(plan, left, left_schema, right_schema),
            side_of(plan, right, left_schema, right_schema),
        ) {
            (Some(Side::Driving), Some(Side::Gathered)) => {
                found.left.push(left);
                found.right.push(right);
            }
            (Some(Side::Gathered), Some(Side::Driving)) => {
                found.left.push(right);
                found.right.push(left);
            }
            // Both operands over one side, which is a predicate that should have been pushed into
            // that side and is not this operator's to be clever about, or an operand over both
            // sides or over neither, which no table can be keyed on. Either way it is a condition
            // on a pair, which is what the residual is.
            _ => {
                found.residual.push(condition);
                continue;
            }
        }
        found.null_is_a_value.push(op == CompareOp::NotDistinctFrom);
    }
    (!found.left.is_empty()).then_some(found)
}

/// The gathered side's rows, in a table that finds them by the values the key expressions produce.
///
/// A chunk at a time, and every part of what a chunk costs is a pass over a column rather than a
/// walk over a row: the key expressions are evaluated through the vectorized evaluator the rest of
/// this file uses, the hash is one pass per key column with the column's type matched on once, and
/// the probe walks the rows of a chunk together so the cache misses on a table larger than the
/// cache are outstanding at the same time. What this replaced built a `Vec<Value>` per gathered row
/// and hashed it a tagged value at a time. See [`Lookup`] for the rest of the argument.
///
/// The chunks are the chunks the pipeline on the other side of the dependency edge produced, held
/// as they were given, so nothing is packed or unpacked here at all. What is charged is the table,
/// after each chunk and by the difference, so a build that is going to be too large says so while
/// it is building rather than at the last row.
fn lookup(
    keying: Keying<'_>,
    chunks: &[Chunk],
    cancel: &Cancel,
    scratch: &mut Reservation,
) -> Result<Lookup> {
    let Keying { plan, exprs, schema, nulls, time_zone } = keying;
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let mut lookup = Lookup::new(rows)?;
    let mut charged = lookup.footprint();
    scratch.grow(charged)?;
    let mut base = 0;
    for chunk in chunks {
        // Once per chunk rather than once per row. A build over a side nobody bounded is the one
        // part of this operator that can run long without producing anything, and a check every two
        // thousand rows is the same granularity the rest of the operator uses.
        cancel.check()?;
        let columns = evaluate_all_in_time_zone(plan, exprs, schema, chunk, time_zone)?;
        lookup.add(&columns, chunk.len(), base, nulls)?;
        let want = lookup.footprint();
        scratch.grow(want.saturating_sub(charged))?;
        charged = want;
        base += chunk.len();
    }
    lookup.seal();
    scratch.shrink(charged.saturating_sub(lookup.footprint()));
    Ok(lookup)
}

/// Whether any row of this side has a null anywhere in its key.
///
/// One pass over the side that is about to go into the table, in the chunks it is already held in
/// and through the evaluator the build uses, reading a validity mask rather than values. What asks
/// is [`Join::marks`], where a null on the gathered side is what turns a miss from false into null.
/// That is a fact about the side and not about any row of it, which is why it is asked once.
fn any_null_key(keying: Keying<'_>, chunks: &[Chunk], cancel: &Cancel) -> Result<bool> {
    let Keying { plan, exprs, schema, time_zone, .. } = keying;
    for chunk in chunks {
        cancel.check()?;
        let columns = evaluate_all_in_time_zone(plan, exprs, schema, chunk, time_zone)?;
        if columns.iter().any(|column| column.validity().has_nulls(chunk.len())) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Everything a pipeline breaker put in a buffer, as one list.
///
/// A join reads its gathered side by position and reads it more than once, so it wants the whole
/// list rather than the shared cursor a pipeline hands out. [`Buffered::reader`] is the handle with
/// a cursor of its own and this is the only thing the join asks of it.
///
/// # Errors
///
/// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while holding the
/// list, or if a chunk that was counted is not there.
fn held(chunks: &Buffered) -> Result<Vec<Chunk>> {
    let reader = chunks.reader();
    (0..reader.len()?)
        .map(|at| {
            reader.at(at)?.ok_or_else(|| {
                Error::internal("a join was given fewer gathered chunks than it was told about")
            })
        })
        .collect()
}

/// Every driving row's slot in the table, in the order the rows are in, [`MISS`] where it has none.
///
/// What the sink needs and the stream does not. [`Probe`] has a driving chunk in hand and looks it
/// up as it arrives, while [`Join`] walks driving rows it gathered earlier and has no chunk to read
/// them out of, so it does the lookup for the whole side first and keeps the answer.
///
/// One `usize` per driving row rather than the row's key, which is what this used to keep. A key was
/// a `Vec<Value>` per row, so a nested loop join over a million driving rows on a two column key
/// held two million tagged values and asked the allocator for a million vectors to put them in, all
/// of it to be thrown away at the end. The slot is what the loop actually reads.
///
/// Rows go back into chunks so that the key columns are produced by the same vectorized evaluator
/// the nested loop's conditions go through. The alternative is an interpreter that walks one
/// expression over one row, which is a second evaluator that has to agree with the first about
/// every cast and every overflow, and two evaluators that are meant to agree is the kind of pair
/// that eventually does not.
fn found(
    keying: Keying<'_>,
    index: &Lookup,
    rows: &[Vec<Value>],
    cancel: &Cancel,
    scratch: &mut Reservation,
) -> Result<Vec<usize>> {
    let Keying { plan, exprs, schema, nulls, time_zone } = keying;
    let types = schema.types();
    let mut built = Vec::with_capacity(rows.len());
    scratch
        .grow(u64::try_from(rows.len().saturating_mul(size_of::<usize>())).unwrap_or(u64::MAX))?;
    let mut probing = Scratch::default();
    let mut slots = Vec::new();
    for batch in rows.chunks(VECTOR_SIZE) {
        // Once per batch rather than once per row. A lookup over a side nobody bounded is one of the
        // two parts of this operator that can run long without producing anything, and a check every
        // two thousand rows is the same granularity the rest of the operator uses.
        cancel.check()?;
        let chunk = rows::pack(&types, batch)?;
        let columns = evaluate_all_in_time_zone(plan, exprs, schema, &chunk, time_zone)?;
        index.slots(&columns, batch.len(), nulls, &mut probing, &mut slots);
        built.extend_from_slice(&slots);
    }
    Ok(built)
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
    use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Plan, Slice};
    use rudb_vector::{Data, Vector};

    use super::{
        Buffered, Chunk, CrossProduct, Gathered, Join, Probe, Progress, Schema, Side, Sink, Stream,
        equalities, side_of,
    };
    use crate::gather::Keep;

    fn chunk(values: &[i32]) -> Chunk {
        let column = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn wide_chunk(values: &[i64]) -> Chunk {
        let column = Vector::flat(LogicalType::BigInt, Data::Int64(values.to_vec().into()))
            .expect("big integers are an i64 layout");
        Chunk::new(vec![column]).expect("one column is one length")
    }

    fn schema(name: &str, table: u32) -> Schema {
        Schema::numbered(vec![Field::new(name, LogicalType::Integer)], table)
    }

    fn typed_schema(name: &str, table: u32, ty: LogicalType) -> Schema {
        Schema::numbered(vec![Field::new(name, ty)], table)
    }

    /// A reference to the only column of the side numbered `table`.
    fn column(plan: &mut Plan, table: u32, ty: LogicalType) -> ExprRef {
        plan.add_expr(Expr::Column(ColumnBinding::new(table, 0)), ty)
    }

    /// `left = right`, which is the one condition shape a lookup answers.
    fn equal(plan: &mut Plan, left: ExprRef, right: ExprRef) -> ExprRef {
        plan.add_expr(Expr::Compare { op: CompareOp::Equal, left, right }, LogicalType::Boolean)
    }

    /// `left > right`, which is a residual wherever it turns up.
    fn greater(plan: &mut Plan, left: ExprRef, right: ExprRef) -> ExprRef {
        plan.add_expr(Expr::Compare { op: CompareOp::Greater, left, right }, LogicalType::Boolean)
    }

    /// A side of two integer columns, `k` to join on and `g` for the condition beside it.
    fn pair_schema(table: u32) -> Schema {
        Schema::numbered(
            vec![Field::new("k", LogicalType::Integer), Field::new("g", LogicalType::Integer)],
            table,
        )
    }

    /// A reference to the column at `position` of the side numbered `table`.
    fn column_at(plan: &mut Plan, table: u32, position: u32, ty: LogicalType) -> ExprRef {
        plan.add_expr(Expr::Column(ColumnBinding::new(table, position)), ty)
    }

    fn pair_chunk(rows: &[(i32, i32)]) -> Chunk {
        let each = |pick: fn(&(i32, i32)) -> i32| {
            Vector::flat(
                LogicalType::Integer,
                Data::Int32(rows.iter().map(pick).collect::<Vec<i32>>().into()),
            )
            .expect("integers are an i32 layout")
        };
        Chunk::new(vec![each(|row| row.0), each(|row| row.1)]).expect("two columns of one length")
    }

    /// A driving side of `INTEGER` and a gathered side of `BIGINT`, which is what makes a cast.
    fn sides() -> (Schema, Schema) {
        (typed_schema("a", 0, LogicalType::Integer), typed_schema("b", 1, LogicalType::BigInt))
    }

    /// Every output chunk of one probe over one driving chunk, in the order it produced them.
    fn probed(probe: &Probe<'_>, driving: &Chunk, width: usize) -> Vec<Vec<Value>> {
        let mut local = probe.local();
        let mut chunk = driving.clone();
        let mut out = Vec::new();
        loop {
            let progress = probe.push(&mut chunk, &mut local).expect("a chunk");
            out.extend((0..chunk.len()).map(|row| {
                (0..width).map(|column| chunk.value_at(row, column)).collect::<Vec<Value>>()
            }));
            if progress != Progress::Again {
                return out;
            }
            chunk = Chunk::empty(&[]);
        }
    }

    /// The right side of a join, run to the end the way the pipeline before this one would.
    fn gathered(memory: &Memory, values: &[i32]) -> (Keep<'static>, Buffered) {
        let (keep, chunks) = Keep::new(memory);
        let mut local = keep.local();
        if !values.is_empty() {
            keep.sink(&chunk(values), &mut local).expect("the right rows");
        }
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        (keep, chunks)
    }

    /// The one left chunk through the sink, and the answer out of the other end.
    fn run(join: &Join<'_>, left: &[i32]) {
        let mut local = join.local();
        if !left.is_empty() {
            join.sink(&chunk(left), &mut local).expect("the left rows");
        }
        join.combine(local).expect("the one instance");
        join.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");
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
            Gathered { schema: &schema("b", 1), chunks: right, marker: None, swapped: false },
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
            Gathered {
                schema: &schema("a", 0),
                chunks: gathered_side,
                marker: None,
                swapped: true,
            },
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
            Gathered {
                schema: &schema("a", 0),
                chunks: gathered_side,
                marker: None,
                swapped: true,
            },
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
            Gathered { schema: &schema("b", 1), chunks: right, marker: None, swapped: false },
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
            Gathered { schema: &schema("b", 1), chunks: right, marker: None, swapped: false },
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
    fn kept(memory: &Memory, first: &[i32], second: &[i32]) -> (Keep<'static>, Buffered) {
        let (keep, out) = Keep::new(memory);
        let mut local = keep.local();
        keep.sink(&chunk(first), &mut local).expect("the first right chunk");
        keep.sink(&chunk(second), &mut local).expect("the second");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
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
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("no chunks");
        let cross = CrossProduct::new(&schema("a", 0), &schema("b", 1), right);

        let mut local = cross.local();
        let mut chunk = chunk(&[1, 2]);
        assert_eq!(cross.push(&mut chunk, &mut local).expect("no rows"), Progress::More);
        assert!(chunk.is_empty());
    }

    #[test]
    fn an_equality_between_two_columns_of_opposite_sides_is_a_key() {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), schema("b", 1));
        let one = column(&mut plan, 0, LogicalType::Integer);
        let other = column(&mut plan, 1, LogicalType::Integer);
        let condition = equal(&mut plan, one, other);

        let found = equalities(&plan, &[condition], &left, &right).expect("a key");

        assert_eq!(found.left, [one]);
        assert_eq!(found.right, [other]);
        assert_eq!(found.null_is_a_value, [false]);
    }

    /// The shape the binder actually produces for a join between columns of different widths.
    ///
    /// Before the key was an expression this join fell to the nested loop, which on the sizes
    /// `optimizer/table_filters.test` uses is ten to the eleventh pairs and hours of work for an
    /// answer the table gives in a second.
    #[test]
    fn a_cast_around_one_operand_is_still_a_key() {
        let mut plan = Plan::new();
        let (left, right) = sides();
        let narrow = column(&mut plan, 0, LogicalType::Integer);
        let widened =
            plan.add_expr(Expr::Cast { input: narrow, try_cast: false }, LogicalType::BigInt);
        let other = column(&mut plan, 1, LogicalType::BigInt);
        let condition = equal(&mut plan, widened, other);

        let found = equalities(&plan, &[condition], &left, &right).expect("a key");

        assert_eq!(found.left, [widened]);
        assert_eq!(found.right, [other]);
    }

    /// Which operand the query wrote first says nothing about which side it reads.
    #[test]
    fn the_gathered_side_written_first_is_lined_back_up() {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), schema("b", 1));
        let driving = column(&mut plan, 0, LogicalType::Integer);
        let gathered = column(&mut plan, 1, LogicalType::Integer);
        let condition = equal(&mut plan, gathered, driving);

        let found = equalities(&plan, &[condition], &left, &right).expect("a key");

        assert_eq!(found.left, [driving]);
        assert_eq!(found.right, [gathered]);
    }

    /// A predicate over one side that the optimizer left on the join rather than pushing down.
    #[test]
    fn an_equality_whose_operands_read_one_side_is_not_a_key() {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), schema("b", 1));
        let one = column(&mut plan, 0, LogicalType::Integer);
        let condition = equal(&mut plan, one, one);

        assert!(equalities(&plan, &[condition], &left, &right).is_none());
    }

    #[test]
    fn an_equality_between_two_constants_is_not_a_key() {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), schema("b", 1));
        let one = plan.add_constant(Value::Integer(1));
        let condition = equal(&mut plan, one, one);

        assert!(equalities(&plan, &[condition], &left, &right).is_none());
    }

    /// An operand over both sides is a comparison in disguise and there is no value to key on.
    #[test]
    fn an_operand_that_reads_both_sides_is_not_one_sides_key() {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), schema("b", 1));
        let one = column(&mut plan, 0, LogicalType::Integer);
        let other = column(&mut plan, 1, LogicalType::Integer);
        let name = plan.intern("+");
        let args = plan.add_expr_list(&[one, other]);
        let sum = plan.add_expr(Expr::Function { name, args }, LogicalType::Integer);

        assert_eq!(side_of(&plan, one, &left, &right), Some(Side::Driving));
        assert_eq!(side_of(&plan, other, &left, &right), Some(Side::Gathered));
        assert_eq!(side_of(&plan, sum, &left, &right), None);
    }

    /// The shape the rest of `optimizer/table_filters.test` is written in.
    #[test]
    fn an_equality_beside_an_inequality_is_still_a_key() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
        let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
        let key = equal(&mut plan, one, other);
        let above = column_at(&mut plan, 0, 1, LogicalType::Integer);
        let below = column_at(&mut plan, 1, 1, LogicalType::Integer);
        let beside = greater(&mut plan, above, below);

        let found = equalities(&plan, &[key, beside], &left, &right).expect("a key");

        assert_eq!(found.left, [one]);
        assert_eq!(found.right, [other]);
        assert_eq!(found.residual, [beside]);
    }

    /// Nothing to narrow to, so there is nothing for a residual to be a residual of.
    #[test]
    fn a_condition_with_no_equality_in_it_is_not_a_key() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
        let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
        let condition = greater(&mut plan, one, other);

        assert!(equalities(&plan, &[condition], &left, &right).is_none());
    }

    #[test]
    fn a_join_with_no_condition_at_all_is_not_a_key() {
        let plan = Plan::new();

        assert!(equalities(&plan, &[], &schema("a", 0), &schema("b", 1)).is_none());
    }

    /// The whole of it: the equality finds the candidates and the inequality throws some away.
    #[test]
    fn a_probe_evaluates_the_rest_of_the_condition_on_the_pairs_the_lookup_found() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let key = {
            let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            equal(&mut plan, one, other)
        };
        let beside = {
            let one = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 1, LogicalType::Integer);
            greater(&mut plan, one, other)
        };
        let conditions = plan.add_expr_list(&[key, beside]);

        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&pair_chunk(&[(2, 5), (2, 50), (3, 5)]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let probe = Probe::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: rows, marker: None, swapped: false },
            JoinKind::Inner,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to look up");

        // (2, 10) finds both gathered rows keyed 2 and keeps the one whose g it is above, (3, 10)
        // finds one and keeps it, and (1, 10) finds none.
        assert_eq!(
            probed(&probe, &pair_chunk(&[(1, 10), (2, 10), (3, 10)]), 4),
            [
                vec![Value::Integer(2), Value::Integer(10), Value::Integer(2), Value::Integer(5)],
                vec![Value::Integer(3), Value::Integer(10), Value::Integer(3), Value::Integer(5)],
            ]
        );
    }

    /// A left join keeps a driving row the residual emptied, not just one the lookup missed.
    #[test]
    fn a_left_join_pads_a_driving_row_the_residual_threw_every_candidate_away_for() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let key = {
            let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            equal(&mut plan, one, other)
        };
        let beside = {
            let one = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 1, LogicalType::Integer);
            greater(&mut plan, one, other)
        };
        let conditions = plan.add_expr_list(&[key, beside]);

        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&pair_chunk(&[(2, 50)]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let probe = Probe::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: rows, marker: None, swapped: false },
            JoinKind::Left,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to look up");

        assert_eq!(
            probed(&probe, &pair_chunk(&[(2, 10)]), 4),
            [vec![Value::Integer(2), Value::Integer(10), Value::Null, Value::Null]]
        );
    }

    /// What one probe over `left` and `right` offers the scan under its driving side.
    fn offered(
        plan: &Plan,
        left: &Schema,
        right: &Schema,
        kind: JoinKind,
        conditions: Slice,
    ) -> Option<(ExprRef, ColumnBinding)> {
        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let local = keep.local();
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let probe = Probe::new(
            plan,
            left,
            &Gathered { schema: right, chunks: rows, marker: None, swapped: false },
            kind,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to look up");

        probe.sideways()
    }

    /// The plain shape, which is the one the filter is for: the gathered side's key to measure the
    /// range over, and the driving column the scan is to be told about.
    #[test]
    fn an_inner_join_on_a_column_offers_its_key_to_the_scan_below() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
        let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
        let condition = equal(&mut plan, one, other);
        let conditions = plan.add_expr_list(&[condition]);

        assert_eq!(
            offered(&plan, &left, &right, JoinKind::Inner, conditions),
            Some((other, ColumnBinding::new(0, 0)))
        );
        assert_eq!(
            offered(&plan, &left, &right, JoinKind::Semi, conditions),
            Some((other, ColumnBinding::new(0, 0))),
            "a semi join drops an unmatched driving row too"
        );
    }

    /// A left join answers with a driving row that matched nothing, so a scan that dropped that row
    /// would lose it from the result.
    #[test]
    fn a_join_that_keeps_an_unmatched_driving_row_offers_nothing() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
        let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
        let condition = equal(&mut plan, one, other);
        let conditions = plan.add_expr_list(&[condition]);

        for kind in [JoinKind::Left, JoinKind::Anti, JoinKind::Single] {
            assert_eq!(offered(&plan, &left, &right, kind, conditions), None, "{kind:?}");
        }
    }

    /// Two nulls match under `IS NOT DISTINCT FROM`, and a range is about order, which has nothing
    /// to say about a null.
    #[test]
    fn an_equality_that_matches_two_nulls_offers_nothing() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
        let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
        let condition = plan.add_expr(
            Expr::Compare { op: CompareOp::NotDistinctFrom, left: one, right: other },
            LogicalType::Boolean,
        );
        let conditions = plan.add_expr_list(&[condition]);

        assert_eq!(offered(&plan, &left, &right, JoinKind::Inner, conditions), None);
    }

    /// A range about the values in a column says nothing about what an expression over that column
    /// produces, so only a plain column is offered.
    #[test]
    fn a_driving_key_that_is_an_expression_offers_nothing() {
        let mut plan = Plan::new();
        let (left, right) = sides();
        let narrow = column(&mut plan, 0, LogicalType::Integer);
        let widened =
            plan.add_expr(Expr::Cast { input: narrow, try_cast: false }, LogicalType::BigInt);
        let other = column(&mut plan, 1, LogicalType::BigInt);
        let condition = equal(&mut plan, widened, other);
        let conditions = plan.add_expr_list(&[condition]);

        assert_eq!(offered(&plan, &left, &right, JoinKind::Inner, conditions), None);
    }

    /// The sink half, where a gathered row counts as matched only once the residual has had it.
    ///
    /// A `FULL` join has to produce the gathered rows nothing kept, so marking one matched on the
    /// lookup alone would swallow a row whose only candidate the residual threw away.
    #[test]
    fn a_full_join_reports_a_gathered_row_whose_only_candidate_the_residual_threw_away() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let key = {
            let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            equal(&mut plan, one, other)
        };
        let beside = {
            let one = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 1, LogicalType::Integer);
            greater(&mut plan, one, other)
        };
        let conditions = plan.add_expr_list(&[key, beside]);

        let memory = Memory::unlimited();
        let (keep, gathered) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&pair_chunk(&[(2, 50), (4, 1)]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let (join, out) = Join::new(
            &plan,
            &left,
            Gathered { schema: &right, chunks: gathered, marker: None, swapped: false },
            JoinKind::Full,
            conditions,
            &Cancel::new(),
            &memory,
        );

        let mut local = join.local();
        join.sink(&pair_chunk(&[(2, 10)]), &mut local).expect("the driving rows");
        join.combine(local).expect("the one instance");
        join.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(
            rows(&out, 4),
            [
                vec![Value::Integer(2), Value::Integer(10), Value::Null, Value::Null],
                vec![Value::Null, Value::Null, Value::Integer(2), Value::Integer(50)],
                vec![Value::Null, Value::Null, Value::Integer(4), Value::Integer(1)],
            ]
        );
    }

    /// The whole of it, from a plan the binder could have produced to the rows that come out.
    #[test]
    fn a_probe_answers_a_join_whose_key_is_a_cast() {
        let mut plan = Plan::new();
        let (left, right) = sides();
        let narrow = column(&mut plan, 0, LogicalType::Integer);
        let widened =
            plan.add_expr(Expr::Cast { input: narrow, try_cast: false }, LogicalType::BigInt);
        let other = column(&mut plan, 1, LogicalType::BigInt);
        let condition = equal(&mut plan, widened, other);
        let conditions = plan.add_expr_list(&[condition]);

        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&wide_chunk(&[2, 3, 4]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let probe = Probe::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: rows, marker: None, swapped: false },
            JoinKind::Inner,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("a lookup answers an inner join on one equality");

        assert_eq!(
            probed(&probe, &chunk(&[1, 2, 3]), 2),
            [vec![Value::Integer(2), Value::BigInt(2)], vec![Value::Integer(3), Value::BigInt(3)]]
        );
    }
}
