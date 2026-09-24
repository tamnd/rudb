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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{
    Cancel, Error, LogicalType, Memory, Reservation, Result, Session, SessionTimeZone, Value,
};
use rudb_kernels::{Connective, combine, is_true};
use rudb_metrics::{Algorithm, Counters, Declined, Joined};
use rudb_pipeline::{Lease, Progress, Sink, Stream};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Plan, Slice};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::buffer::Buffered;
use crate::expr::evaluate_all_in_time_zone;
use crate::extents::{Extents, Spread};
use crate::gather::{self, Gathering};
use crate::lookup::{Lookup, MISS, NONE, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::side::{Build, PAD, laid_out};
use crate::sideways::Sideways;

/// Why a join that found a key did not walk every pair, for the metrics document.
const KEYED: &str = "a conjunct of the condition is an equality with one side's columns on each \
                     side of it, so a driving row's matches are one lookup rather than a pass over \
                     the gathered side";

/// Why a join that found no key had to walk every pair.
///
/// This is the sentence worth finding in a slow query. A nested loop over two sides of any size is
/// the two multiplied, and the fix is almost always a condition the binder could not see an
/// equality in rather than anything about the data.
const UNKEYED: &str = "no conjunct of the condition is an equality with one side's columns on each \
                       side of it, so there is no key to build a table on";

/// Why a mark join with a key still walked every pair.
const MARK_IS_NARROW: &str = "a mark join is answered from a table only when its condition is one \
                              equality with nothing left over, because its answer has to tell a \
                              miss apart from a pair nobody could decide";

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
    /// Where the build side is reported. See [`Probe::counters`].
    counters: Option<Arc<Counters>>,
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
            counters: None,
        };
        (join, out)
    }

    /// Applies the session semantics to join conditions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.time_zone = session.session_time_zone();
        self
    }

    /// Reports the build side and the algorithm. See [`Probe::watched`].
    #[must_use]
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// What this operator produces, which is both sides' columns unless the kind throws one away.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// What this operator did, for the row it gets in the metrics document.
    ///
    /// The kinds that stay on this operator are the ones a lookup cannot answer on its own, so the
    /// nested loop is the usual answer here and a reader of a slow query wants to be told that
    /// rather than left to work it out from the time. See [`Counters::joining`].
    fn reporting(&self, algorithm: Algorithm, build_rows: usize, build_bytes: u64) {
        let Some(counters) = &self.counters else {
            return;
        };
        let declined = match algorithm {
            Algorithm::Hash => vec![Declined::new(Algorithm::Loop, KEYED)],
            Algorithm::Loop if self.kind == JoinKind::Mark => {
                vec![Declined::new(Algorithm::Hash, MARK_IS_NARROW)]
            }
            Algorithm::Loop => vec![Declined::new(Algorithm::Hash, UNKEYED)],
            // Nothing else was ever in the running. A positional join pairs the nth row of one
            // side with the nth of the other, which is not a search for anything, so there is no
            // algorithm here to have preferred.
            Algorithm::Positional => Vec::new(),
        };
        counters.joining(Joined {
            algorithm,
            build_rows: u64::try_from(build_rows).unwrap_or(u64::MAX),
            build_bytes,
            declined,
        });
    }

    /// The joined rows, before they are turned back into chunks.
    fn joined(
        &self,
        left_rows: &[Vec<Value>],
        right_chunks: &[Chunk],
        threads: &Lease<'_>,
    ) -> Result<Vec<Vec<Value>>> {
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
        let right = Build::new(&right_types, right_chunks, threads)?;
        scratch.grow(right.footprint())?;
        let right_rows = right.rows();
        if self.kind == JoinKind::Positional {
            self.reporting(Algorithm::Positional, right_rows, scratch.bytes());
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
            // On one thread, because this is the row major path that `RIGHT` and `FULL` are still
            // on and it has no lease to spend. The streaming operator next door has one and the
            // build there is the one that matters.
            Some(equalities) => Some(lookup(
                equalities.gathered(self.plan, &self.right_schema, self.time_zone),
                right_chunks,
                &self.cancel,
                &Lease::alone(),
                &mut scratch,
            )?),
            None => None,
        };
        // Here rather than when the operator was built, because what decides the algorithm is
        // whether the condition holds an equality this can key on, and that question is answered
        // on the two lines above. Deciding it a second time in the code that builds the operator
        // would be a second answer that can disagree with this one.
        let keyed = index.is_some() || marks.is_some();
        self.reporting(
            if keyed { Algorithm::Hash } else { Algorithm::Loop },
            right_rows,
            scratch.bytes(),
        );
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
            // Nothing, which reads as every column wanted. This operator answers a pair at a time
            // through [`Residual::keep`], which never asks, and an empty list is the answer that
            // cannot be wrong if it ever does.
            wanted: &[],
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
        let index = lookup(gathered, right_chunks, &self.cancel, &Lease::alone(), scratch)?;
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

    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let left_rows = std::mem::take(&mut *self.left.lock().map_err(poisoned)?);
        let right_chunks = held(&self.right)?;
        let mut out = self.joined(&left_rows, &right_chunks, threads)?;
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

/// A single join with no condition, which is what an uncorrelated scalar subquery becomes.
///
/// TPC-H q22 compares every customer's balance with one average, and the plan says that as a single
/// join of the customers against the one row the average comes out as. The nested loop [`Join`]
/// answered it by gathering every customer as a row of values on one thread and pairing each one
/// with the build side, which was 18 ms of a 23 ms query. With no condition there is nothing to
/// search for: every driving row gets the one gathered row, or nulls when there is none, and more
/// than one is the error a scalar subquery raises. So this reads the gathered side once and puts
/// its row beside each driving chunk as constant columns, in the pipeline the driving rows came
/// from and on every thread it has.
#[derive(Debug)]
pub(crate) struct Broadcast {
    types: Vec<LogicalType>,
    right_types: Vec<LogicalType>,
    schema: Schema,
    /// The gathered side, filled by the pipeline this one depends on.
    right: Buffered,
    /// The gathered row, or nulls for none, and `None` when there were too many to pick one.
    row: OnceLock<Option<Vec<Value>>>,
}

impl Broadcast {
    /// `right` is the handle on the chunks the other pipeline kept.
    pub(crate) fn new(left: &Schema, right_schema: &Schema, right: Buffered) -> Self {
        let schema = Schema::concat(left, right_schema);
        Self {
            types: schema.types(),
            right_types: right_schema.types(),
            schema,
            right,
            row: OnceLock::new(),
        }
    }

    /// What this operator produces, which is both sides' columns.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The row every driving row is given, read out of the gathered side the first time.
    fn row(&self) -> Result<&Option<Vec<Value>>> {
        if let Some(row) = self.row.get() {
            return Ok(row);
        }
        let chunks = held(&self.right)?;
        let mut rows = chunks.iter().filter(|chunk| !chunk.is_empty());
        let row = match (rows.next(), rows.next()) {
            (None, _) => Some(vec![Value::Null; self.right_types.len()]),
            (Some(chunk), None) if chunk.len() == 1 => Some(chunk.row(0).collect()),
            _ => None,
        };
        Ok(self.row.get_or_init(|| row))
    }
}

impl Stream for Broadcast {
    type Local = ();

    fn local(&self) {}

    /// More than one gathered row is an error whether or not a driving row ever arrives, which is
    /// what DuckDB does: `SELECT count(*) FROM empty WHERE x > (SELECT a FROM t)` raises it.
    fn prepare(&self, _threads: &Lease<'_>) -> Result<()> {
        match self.row()? {
            Some(_) => Ok(()),
            None => Err(too_many_rows()),
        }
    }

    fn push(&self, chunk: &mut Chunk, _local: &mut ()) -> Result<Progress> {
        let rows = chunk.len();
        if rows == 0 {
            *chunk = Chunk::empty(&self.types);
            return Ok(Progress::More);
        }
        let Some(row) = self.row()? else {
            return Err(too_many_rows());
        };
        let mut columns = std::mem::replace(chunk, Chunk::empty(&[])).into_columns();
        columns.extend(
            row.iter()
                .zip(&self.right_types)
                .map(|(value, ty)| Vector::constant(ty.clone(), value.clone(), rows)),
        );
        *chunk = Chunk::with_rows(columns, rows)?;
        Ok(Progress::More)
    }
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
/// What it cannot do is the kinds whose answer depends on which driving rows came before. A `RIGHT`
/// or a `FULL` join keeps the gathered rows nothing matched, and which those are is not known until
/// every driving row has been through, so those stay on [`Join`] with its `finalize`. A
/// `POSITIONAL` one is not a lookup at all. [`streamed`] is the list.
///
/// A `MARK` join does ask a question of the whole gathered side, and it is still here, because the
/// gathered side is finished before the first driving row arrives. See [`Built::undecided`].
#[derive(Debug)]
pub(crate) struct Probe<'a> {
    plan: &'a Plan,
    kind: JoinKind,
    equalities: Equalities,
    /// Which of the gathered side's columns the marker goes in, for a mark join. See
    /// [`Gathered::marker`].
    marker: Option<usize>,
    /// What a driving row looks like, which is what the driving key expressions resolve against.
    left_schema: Schema,
    /// What a gathered row looks like, which is what the gathered key expressions resolve against.
    right_schema: Schema,
    /// Both sides' columns in this operator's order, which is what the residual resolves against.
    ///
    /// This operator's order and not the plan's even when swapped, for the reason [`Join::swapped`]
    /// gives: a condition finds its columns by binding rather than by counting.
    combined: Schema,
    /// Which of those columns the residual reads, in the same order.
    ///
    /// A residual is evaluated over a chunk of pairs, and building that chunk means gathering both
    /// sides at the pair list. A conjunct the lookup could not answer usually reads one column of
    /// each side, and everything else in the two schemas is gathered so that the column numbers
    /// still line up. So the ones nothing reads are stood in for by a constant, which lines the
    /// numbers up for nothing rather than for a pass over the pairs. TPC-H q21 is the case: the
    /// residual is one comparison of two `l_suppkey` columns and the driving side carries four.
    wanted: Vec<bool>,
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
    /// Where the build side is reported, for a query somebody is measuring.
    ///
    /// Nothing outside this operator can see the gathered side: the shim that counts rows sees the
    /// driving chunks going past and the row this operator gets in the document would otherwise
    /// have one of its two inputs missing from it. See [`Counters::joining`].
    counters: Option<Arc<Counters>>,
    /// Runtime filters of joins on the driving side, by the key position they say something about.
    ///
    /// See [`Probe::narrowed_by`].
    narrowing: Vec<(usize, Arc<Sideways<'a>>)>,
}

/// The gathered side and the table that finds rows in it.
#[derive(Debug)]
struct Built {
    rows: Build,
    index: Lookup,
    /// Whether a mark join's misses are null rather than false, which is one fact about this side.
    ///
    /// Read the condition as `d = g`. A driving row that hits is true. One that misses is false
    /// unless some pair of it with a gathered row came out null instead, and `d = g` is null
    /// exactly when one of its two operands is. Whether `d` is null is a question about the driving
    /// row and is asked in the loop. Whether any `g` is null is a question about this side and
    /// nothing else, so it is asked once, here, while the table is being built and the driving side
    /// has not started. That is the whole reason a mark join can stream.
    ///
    /// False for every other kind, which never reads it, and false for a mark join written with
    /// `IS NOT DISTINCT FROM`, where nulls are values in the table and a miss is an honest false.
    undecided: bool,
}

/// How many pairs a residual is evaluated over in one go, at most.
///
/// A cap rather than the whole driving chunk, because a chunk of a thousand driving rows against a
/// key a thousand gathered rows share is a million pairs and holding them all would be a burst of
/// memory nothing asked for. Sixteen vectors is large enough that the fixed cost of an evaluation
/// is spread over a full batch even when the pairs come from one driving row at a time, and small
/// enough to stay in cache.
///
/// One driving row is never split across two fills. The cap is checked after a row's candidates
/// have gone in, so a row with more candidates than this produces an oversized batch on its own,
/// which is the same thing the code this replaces did with that row.
const RESIDUAL_BATCH: usize = 16 * VECTOR_SIZE;

/// The candidates a residual kept, for a run of driving rows rather than for one of them.
///
/// The reason this exists is that a residual used to be evaluated once per driving row. Each of
/// those evaluations gathered that row's candidates into a chunk, built a constant vector per
/// driving column to repeat the row across them, ran the expression tree, combined the flags and
/// read the result back. All of that is a fixed cost paid per driving row, and on TPC-H q21 the
/// two joins that carry a residual have about four candidates per driving row, so the fixed cost
/// was paid once per four pairs and the vectors it built were four rows long.
///
/// So the pairs of many driving rows are collected first and the residual is run over a full batch
/// of them. What comes back is the same answer per driving row, held as one run of gathered rows
/// with an index saying where each driving row's share of it begins. The row loop then reads a
/// slice out of that instead of calling the evaluator.
#[derive(Debug, Default)]
struct Candidates {
    /// The gathered rows that survived, every driving row's share laid end to end.
    right: Vec<u32>,
    /// Where each driving row's share of `right` begins, with a last entry for the end.
    ///
    /// Indexed by the driving row less [`Candidates::from`], so it is one longer than the run of
    /// driving rows this covers.
    start: Vec<u32>,
    /// The first driving row this covers.
    from: usize,
    /// One past the last driving row this covers, so `from == to` is nothing.
    to: usize,
    /// The candidates before the residual was asked about them.
    raw: Vec<u32>,
    /// Which driving row each entry of `raw` belongs to, which is what the pairs are gathered at.
    driving: Vec<u32>,
    /// Whether the residual kept each entry of `raw`.
    pass: Vec<bool>,
    /// One driving row's chain, refilled per row while the batch is being collected.
    chain: Vec<u32>,
}

impl Candidates {
    /// Whether `row` is a driving row this already has the answer for.
    fn holds(&self, row: usize) -> bool {
        row >= self.from && row < self.to
    }

    /// The gathered rows the residual kept for driving row `row`.
    ///
    /// Empty for a row this does not cover, which a caller avoids by asking [`Candidates::holds`]
    /// first. It is empty rather than an error because an empty list is what a driving row that
    /// matched nothing has, so a caller that got the range wrong would see a wrong answer either
    /// way and the check belongs where the fill is decided.
    fn of(&self, row: usize) -> &[u32] {
        let Some(index) = row.checked_sub(self.from) else {
            return &[];
        };
        let (Some(&begin), Some(&end)) = (self.start.get(index), self.start.get(index + 1)) else {
            return &[];
        };
        self.right.get(begin as usize..end as usize).unwrap_or_default()
    }

    /// Nothing covered, which is what a new driving chunk means.
    fn forget(&mut self) {
        self.from = 0;
        self.to = 0;
    }

    /// Answers the residual for as many driving rows from `from` as fit in one batch.
    fn fill(
        &mut self,
        residual: &Residual<'_>,
        left: &Chunk,
        built: &Built,
        slots: &[usize],
    ) -> Result<()> {
        self.raw.clear();
        self.driving.clear();
        self.start.clear();
        self.right.clear();
        self.start.push(0);
        let mut row = self.from;
        while row < left.len() {
            let slot = slots.get(row).copied().unwrap_or(MISS);
            built.index.matches(slot, &mut self.chain);
            let at = u32::try_from(row).map_err(|_| too_many_rows())?;
            self.raw.extend_from_slice(&self.chain);
            self.driving.extend(std::iter::repeat_n(at, self.chain.len()));
            row += 1;
            self.start.push(u32::try_from(self.raw.len()).map_err(|_| too_many_rows())?);
            if self.raw.len() >= RESIDUAL_BATCH {
                break;
            }
        }
        self.to = row;
        residual.keeps(left, &built.rows, &self.driving, &self.raw, &mut self.pass)?;
        // The offsets are into `raw` and they become offsets into `right`, which holds a subset of
        // the same entries in the same order and so is never longer. That is what lets the rewrite
        // run in place: an entry is read before the slot it came from is written, and the slot for
        // the next driving row is not touched until the row after it.
        let mut kept: usize = 0;
        for index in 0..self.to - self.from {
            let begin = self.start[index] as usize;
            let end = self.start[index + 1] as usize;
            self.start[index] = u32::try_from(kept).map_err(|_| too_many_rows())?;
            for at in begin..end {
                if self.pass.get(at).copied().unwrap_or(false) {
                    self.right.push(self.raw[at]);
                    kept += 1;
                }
            }
        }
        if let Some(last) = self.start.last_mut() {
            *last = u32::try_from(kept).map_err(|_| too_many_rows())?;
        }
        Ok(())
    }
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
    /// The first gathered row each driving row matches, [`NONE`] for none, one entry per slot.
    ///
    /// Read out of the table for the whole chunk at once. See [`Lookup::firsts`].
    firsts: Vec<u32>,
    /// The buffers that lookup walks the chunk with, held here so that a chunk costs no allocation.
    scratch: Scratch,
    /// The gathered rows the current driving row matches, refilled per row from its chain.
    chain: Vec<u32>,
    /// The candidates a residual condition kept, when there is a residual condition.
    ///
    /// Filled for a run of driving rows rather than for one of them, which is why it is a structure
    /// rather than a list. See [`Candidates`].
    cand: Candidates,
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
/// Every one of these decides about a driving row from that row's matches, plus at most one fact
/// about the gathered side that was settled before any driving row arrived. That is what makes the
/// answer a stream. The rest need something only a whole pass over the driving side knows, and they
/// are on [`Join`].
///
/// `MARK` is on the list for the second half of that first sentence. Its answer is null where a
/// miss cannot be told apart from an unknown, and what decides that is whether the gathered side
/// holds a null key, which is one pass over a side the pipeline before this one already finished.
/// See [`Built::undecided`]. What a lookup still cannot answer is a mark join with more than one
/// equality or with a residual, for the reason [`Join::marks`] gives, and [`Probe::new`] hands
/// those back rather than deciding them wrongly.
pub(crate) fn streamed(kind: JoinKind) -> bool {
    matches!(
        kind,
        JoinKind::Inner
            | JoinKind::Left
            | JoinKind::Semi
            | JoinKind::Anti
            | JoinKind::Single
            | JoinKind::Mark
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
        // The narrower rule a mark join is answered by. One equality and nothing left over, which
        // is what makes a miss decidable from the side alone. [`Join::marks`] is the argument and
        // it is the same argument, so the two places agree by saying the same thing.
        let marker = if kind == JoinKind::Mark {
            if equalities.left.len() != 1 || !equalities.residual.is_empty() {
                return None;
            }
            Some(right.marker.or_else(|| right_schema.bindings().len().checked_sub(1))?)
        } else {
            None
        };
        let schema = match kind {
            JoinKind::Semi | JoinKind::Anti => left.clone(),
            // The plan's order rather than this operator's, for the reason [`Join::new`] gives.
            _ if swapped => Schema::concat(right_schema, left),
            _ => Schema::concat(left, right_schema),
        };
        let combined = Schema::concat(left, right_schema);
        let mut wanted = vec![false; combined.bindings().len()];
        for &expr in &equalities.residual {
            columns(plan, expr, &mut |binding| {
                // A binding the combined schema cannot place is one this cannot rule out, so the
                // whole of that side is gathered as it was. Nothing reaches here with one today,
                // since a residual is a conjunct of the join's own condition, and the refusal is
                // here so that a plan that did would be slow rather than wrong.
                match combined.position_of(binding) {
                    Some(at) => {
                        if let Some(flag) = wanted.get_mut(at) {
                            *flag = true;
                        }
                    }
                    None => wanted.fill(true),
                }
            });
        }
        Some(Self {
            plan,
            kind,
            equalities,
            marker,
            left_schema: left.clone(),
            right_schema: right_schema.clone(),
            combined,
            wanted,
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
            counters: None,
            narrowing: Vec::new(),
        })
    }

    /// The driving columns this join compares with `=`, by the position of the key they are in.
    ///
    /// Only the ones where the driving key is a bare column, because what is asked about it is
    /// whether a join further down already dropped every driving row whose value is outside a set,
    /// and that is a fact about the column rather than about an expression over it.
    pub(crate) fn driving_columns(&self) -> Vec<(usize, ColumnBinding)> {
        let equalities = &self.equalities;
        (0..equalities.null_is_a_value.len())
            .filter(|&at| !equalities.null_is_a_value[at])
            .filter_map(|at| match *self.plan.expr(*equalities.left.get(at)?) {
                Expr::Column(binding) => Some((at, binding)),
                _ => None,
            })
            .collect()
    }

    /// Leaves out of the table every gathered row whose key one of `narrowing` says no driving
    /// row can hold.
    ///
    /// Each entry is the runtime filter of an inner or a semi join on this join's driving side, and
    /// the key position it is about. That join has already dropped every driving row whose key is
    /// outside its build side's keys, so by the time a driving row reaches this one its key is in
    /// that set, and a gathered row whose key is not can match nothing. Leaving it out of the table
    /// changes no answer, because this operator never hands out a gathered row that matched
    /// nothing, and it is what makes the table small. On TPC-H q09 the join to `partsupp` gathers
    /// all eight hundred thousand rows while the driving rows have already been through the join
    /// to green parts on the same part key, so only about one in twenty of them could ever match.
    ///
    /// The builder only passes filters of joins below this one on the driving side, which are the
    /// ones whose build sides have finished by the time this table is built, and only the exact
    /// bitmap is read, since it answers for the integer value whatever the width of the column.
    ///
    /// Not for a mark join. It tells a null driving key apart by whether the gathered side has
    /// rows, and it asks the table, so a table narrowed to nothing would answer false for a row
    /// whose honest answer is null. An outer join between the two can put that null there.
    #[must_use]
    pub(crate) fn narrowed_by(mut self, narrowing: Vec<(usize, Arc<Sideways<'a>>)>) -> Self {
        if self.kind != JoinKind::Mark {
            self.narrowing = narrowing;
        }
        self
    }

    /// Which gathered rows can go in the table, by [`Probe::narrowed_by`], or `None` for all.
    fn allowed(&self, keys: &[Vector], rows: usize) -> Option<Vec<bool>> {
        let mut allowed: Option<Vec<bool>> = None;
        let mut block = Vec::new();
        for (at, sideways) in &self.narrowing {
            let (Some(domain), Some(key)) = (sideways.kept(), keys.get(*at)) else { continue };
            let integer = matches!(
                key.logical_type(),
                LogicalType::TinyInt
                    | LogicalType::SmallInt
                    | LogicalType::Integer
                    | LogicalType::BigInt
            );
            if !integer {
                continue;
            }
            let mask = allowed.get_or_insert_with(|| vec![true; rows]);
            let mut here = vec![false; rows];
            for row in domain.keep(key, rows, &mut block) {
                here[row as usize] = true;
            }
            for (flag, here) in mask.iter_mut().zip(here) {
                *flag = *flag && here;
            }
        }
        allowed
    }

    /// Applies the session semantics to the key expressions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.time_zone = session.session_time_zone();
        self
    }

    /// Reports the build side into the same row the shim counts the driving side into.
    #[must_use]
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
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
    ///
    /// Every equality that qualifies, in the order they were written, because which of them reaches
    /// a scan is a question about the plan below and not about this join. A join on two keys where
    /// one of them came out of an earlier join has only the other to offer.
    pub(crate) fn sideways(&self) -> Vec<(ExprRef, ColumnBinding)> {
        if !matches!(self.kind, JoinKind::Inner | JoinKind::Semi) {
            return Vec::new();
        }
        self.keyed_sideways()
    }

    /// The same, without the question about the kind.
    ///
    /// [`Marking`] answers that question differently and everything else about the key is the
    /// same, so the two refusals that are about the equality live here and the one that is about
    /// what the join does with a driving row lives with each operator.
    fn keyed_sideways(&self) -> Vec<(ExprRef, ColumnBinding)> {
        let equalities = &self.equalities;
        (0..equalities.null_is_a_value.len())
            .filter(|&at| !equalities.null_is_a_value[at])
            .filter_map(|at| {
                let Expr::Column(binding) = *self.plan.expr(*equalities.left.get(at)?) else {
                    return None;
                };
                Some((*equalities.right.get(at)?, binding))
            })
            .collect()
    }

    /// A mark join's answer for one driving chunk, which is that chunk with a marker beside it.
    ///
    /// Every driving row comes out and comes out exactly once, in the order it arrived, so there is
    /// no list of positions to gather at and the driving columns are passed through untouched.
    /// What is added is the gathered side's columns, null in all of them but the marker, which is
    /// where a mark join's answer lives and is the only one of them anything above this reads. That
    /// is the same shape [`Join`] produces a row at a time with [`pad_right`].
    ///
    /// The three valued rule is [`Join::marks`], and the whole of it is here in a form that reads
    /// a slot and a validity bit per row. True where the lookup hit. Where it missed, false unless
    /// some pair of this row with a gathered row came out null instead, which is
    /// [`Built::undecided`] for the gathered half and this row's own key for the driving half.
    fn marked(
        &self,
        chunk: &mut Chunk,
        left: &Chunk,
        built: &Built,
        local: &Probing,
    ) -> Result<Progress> {
        let rows = left.len();
        // A gathered side with no rows in it has no pairs at all, so nothing about it is unknown
        // and every marker is false. That is a different side from one whose keys are all null,
        // which has pairs, answers all of them null, and arrives here with the same empty table.
        let empty = built.rows.rows() == 0;
        // Under `IS NOT DISTINCT FROM` a null key is a value the table holds and matches, so a miss
        // is an honest false and this row's own key has nothing to say. Under `=` it does.
        let driving = match self.equalities.null_is_a_value.first() {
            Some(true) => None,
            // Nothing when the table is empty, because the keys were not evaluated then. See
            // [`Probing::keys`]. Every such row is already decided by `empty` or by `undecided`.
            _ => local.keys.first(),
        };
        let mut marks = vec![false; rows];
        let mut known = vec![true; rows];
        // No check in here. It is one slot read and one validity read per row over a driving chunk
        // of at most [`VECTOR_SIZE`] rows, and the wrapper checks between chunks.
        for (row, (mark, decided)) in marks.iter_mut().zip(known.iter_mut()).enumerate() {
            if local.slots.get(row).copied().unwrap_or(MISS) != MISS {
                *mark = true;
            } else if !empty {
                *decided = !built.undecided && !driving.is_some_and(|key| key.is_null_at(row));
            }
        }
        let marker = Vector::flat(LogicalType::Boolean, Data::Bool(marks.into()))?
            .with_validity(Validity::from_run(&known));
        let mut columns: Vec<Vector> = left.columns().to_vec();
        for logical in &self.right_types {
            columns.push(Vector::constant(logical.clone(), Value::Null, rows));
        }
        let at = self.marker.map_or(usize::MAX, |at| self.left_width + at);
        let Some(slot) = columns.get_mut(at) else {
            return Err(Error::internal("a mark join has no marker column"));
        };
        *slot = marker;
        *chunk = Chunk::with_rows(columns, rows)?;
        Ok(Progress::More)
    }

    /// The conjuncts the lookup did not answer, over a pair of this join's two sides.
    fn residual(&self) -> Residual<'_> {
        Residual {
            plan: self.plan,
            exprs: &self.equalities.residual,
            combined: &self.combined,
            wanted: &self.wanted,
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
    ///
    /// In a parallel run nobody reaches this with anything to do, because [`Stream::prepare`] has
    /// already filled it with the whole lease rather than with one instance. It is still here and
    /// still correct on its own, because a `Probe` reached any other way, which is what every test
    /// in this file does, has no lease to be given.
    fn built(&self) -> Result<Arc<Built>> {
        self.built_with(&Lease::alone())
    }

    /// The same, on the threads the pipeline holding this operator leased.
    ///
    /// Both of the two pieces are parallel now. A column of the gathered side depends on that
    /// column alone, so laying the chunks end to end is a task per column, and the table is built
    /// in partitions of the hash, which is a task per partition. See [`laid_out`] and
    /// [`Lookup::build`] for what each of them does with the lease.
    ///
    /// Evaluating the keys was tried as a task per chunk and taken out again. The keys of a TPC-H
    /// join are bare column references, so the work per chunk is close to nothing, and against that
    /// a lock per chunk plus every chunk's keys held at once was slower than doing it in the loop.
    fn built_with(&self, threads: &Lease<'_>) -> Result<Arc<Built>> {
        self.built
            .get_or_init(|| {
                let (chunks, kept) = self.gathered.take()?;
                let keying =
                    self.equalities.gathered(self.plan, &self.right_schema, self.time_zone);
                let mut charged = self.held.lock().map_err(poisoned)?;
                // Before the table rather than after it, because it is one pass over the same
                // chunks and reading them while they are warm costs less than reading them twice.
                // Only a mark join asks, and only one written with `=`. See [`Built::undecided`].
                let undecided = self.kind == JoinKind::Mark
                    && !self.equalities.null_is_a_value.first().copied().unwrap_or(false)
                    && any_null_key(keying, &chunks, &self.cancel)?;
                // The chunks laid end to end, which is a copy of the side and is charged as one.
                // The chunks themselves are not charged again here: `kept` is what the keep that
                // made them charged, and it goes when they do.
                let rows = Build::new(&self.right_types, &chunks, threads)?;
                charged.grow(rows.footprint())?;
                // Laid before the table rather than after it, because a key that is a column of
                // this side is already laid out in `rows` and the table reads it from there.
                let index = match laid_keys(keying, &rows) {
                    Some(keys) => {
                        // Everything the table reads is in `rows` now, so the chunks go before
                        // the table is built rather than after the join is done.
                        drop(chunks);
                        drop(kept);
                        let allowed = self.allowed(&keys, rows.rows());
                        let index = Lookup::build_among(
                            &keys,
                            rows.rows(),
                            keying.nulls,
                            allowed.as_deref(),
                            threads,
                            &self.cancel,
                        )?;
                        charged.grow(index.footprint())?;
                        index
                    }
                    None => lookup(keying, &chunks, &self.cancel, threads, &mut charged)?,
                };
                if let Some(counters) = &self.counters {
                    counters.joining(Joined {
                        algorithm: Algorithm::Hash,
                        build_rows: u64::try_from(rows.rows()).unwrap_or(u64::MAX),
                        build_bytes: charged.bytes(),
                        declined: vec![Declined::new(Algorithm::Loop, KEYED)],
                    });
                }
                Ok(Arc::new(Built { rows, index, undecided }))
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
            firsts: Vec::new(),
            scratch: Scratch::default(),
            chain: Vec::new(),
            cand: Candidates::default(),
            left_at: Vec::new(),
            right_at: Vec::new(),
            row: 0,
            hit: 0,
        }
    }

    /// Builds the table here, where the whole lease is free, rather than inside an instance.
    ///
    /// Before this the first instance to call [`Probe::built`] built it and the rest of the lease
    /// slept on the lock. On TPC-H q9 that was 5169 of 11254 worker samples in `semaphore_wait_trap`
    /// and 46 percent of all worker thread time, for a build that is 17 percent of the query.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        self.built_with(threads)?;
        Ok(())
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
                // What a residual answered about the chunk before this one says nothing about this
                // one, and the row numbers it is held under would be read as if it did.
                local.cand.forget();
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
                built.index.firsts(&local.slots, &mut local.firsts);
                left
            }
        };
        // Before the loop below rather than an arm inside it, because a mark join answers a whole
        // driving chunk at once and the loop is written around a row producing some number of
        // output rows. It never asks again, so nothing of the chunk is held over.
        if self.kind == JoinKind::Mark {
            return self.marked(chunk, &left, &built, local);
        }
        let residual = self.residual();
        {
            // The fields this loop touches, taken apart so that the candidates a residual answered
            // can be held across a push to the answer. They are separate fields and nothing reads
            // two of them at once, but a `local.cand` borrowed while `local.left_at` is written is
            // one borrow of the whole state twice over as far as the compiler is concerned.
            let Probing { slots, firsts, chain, cand, left_at, right_at, row, hit, .. } =
                &mut *local;
            let single = built.index.single();
            // The two halves of the answer, one entry per output row. Nothing is built here but a
            // pair of numbers per pair of rows, and the columns are gathered at those numbers below.
            left_at.clear();
            right_at.clear();
            while *row < left.len() && left_at.len() < VECTOR_SIZE {
                // Once per driving row, the same granularity the nested loop checks at, and the
                // only place in this operator that runs long once the table is built.
                self.cancel.check()?;
                let found: &[u32] = if residual.exprs.is_empty() {
                    // The chain the lookup above left for this row, which is one walk of a run of
                    // `u32` rather than a hash and a map lookup. An ordinary equi join answers out
                    // of it directly and so costs no boxed row and no second pass at all. Against a
                    // table where every key has one row the chain is its first row alone, and that
                    // was read for the whole chunk before this loop started.
                    match firsts.get(*row) {
                        None | Some(&NONE) => &[],
                        Some(first) if single => std::slice::from_ref(first),
                        Some(&first) => {
                            built.index.chain_from(first, chain);
                            chain
                        }
                    }
                } else {
                    // A residual is answered for a batch of driving rows at a time and read back
                    // here a row at a time. The refill covers this row and as many after it as fit,
                    // so the test fails once per batch rather than once per row. See [`Candidates`].
                    if !cand.holds(*row) {
                        cand.from = *row;
                        cand.fill(&residual, &left, &built, slots)?;
                    }
                    cand.of(*row)
                };
                let at = u32::try_from(*row).map_err(|_| too_many_rows())?;
                match self.kind {
                    JoinKind::Semi => {
                        if !found.is_empty() {
                            left_at.push(at);
                        }
                    }
                    JoinKind::Anti => {
                        if found.is_empty() {
                            left_at.push(at);
                        }
                    }
                    JoinKind::Single => {
                        if found.len() > 1 {
                            return Err(too_many_rows());
                        }
                        left_at.push(at);
                        right_at.push(found.first().copied().unwrap_or(PAD));
                    }
                    JoinKind::Left if found.is_empty() => {
                        left_at.push(at);
                        right_at.push(PAD);
                    }
                    // An inner join with no match produces nothing, which is this arm with an empty
                    // list, and the rest of it is one output row per match.
                    _ => {
                        let room = VECTOR_SIZE - left_at.len();
                        let end = (*hit + room).min(found.len());
                        for &found_at in &found[*hit..end] {
                            left_at.push(at);
                            right_at.push(found_at);
                        }
                        if end < found.len() {
                            // A key with more matches than fit in a chunk. The row stays where it
                            // is and the next call picks up from the match this one stopped at.
                            *hit = end;
                            break;
                        }
                        *hit = 0;
                    }
                }
                *row += 1;
            }
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

/// A semi or an anti join that gathered the side whose rows it produces.
///
/// Every other operator here gathers one side and produces rows as the other one streams past. A
/// semi join produces its subject side's rows, so [`Probe`] has to gather the other side, and when
/// the other side is the larger of the two that is the wrong way round. TPC-H q21 is the case that
/// asked for this: its `EXISTS` becomes a semi join whose subject is about seventy five thousand
/// rows and whose other side is the whole six million row `lineitem`, and gathering six million
/// rows to answer seventy five thousand questions is a table thirty times larger than it needs to
/// be, built out of a copy of a side thirty times larger than the answer.
///
/// So this one turns the join around. The subject side is gathered, the other side streams, and a
/// driving row that finds a match sets a bit against the gathered row it matched rather than
/// producing anything. When the driving side is finished, the gathered rows whose bit is set are
/// the semi join's answer and the ones whose bit is clear are the anti join's. It is the same
/// table, the same lookup and the same residual as [`Probe`], which is why it holds one and calls
/// into it rather than spelling any of that a second time. What differs is only what is done with
/// the matches.
///
/// # Why this is a sink
///
/// Because nothing can be said about a gathered row until the last driving row has been through.
/// A bit that is clear now may be set by a driving row that has not arrived, so the answer is not
/// known until the pipeline ends, and an operator whose answer is only known then is a
/// [`Sink`]. That is the same argument [`streamed`] gives for `RIGHT` and `FULL`.
///
/// What it costs is that the answer is materialised instead of streaming on, and what it saves is
/// gathering the larger side. `rudb_opt`'s `sides` pass only turns a join around when it estimates
/// the subject to be the smaller of the two, so the copy that is made is the smaller one and the
/// copy that is avoided is the larger one.
///
/// The bits themselves are per instance and merged in [`Sink::combine`], so the driving side runs
/// on the whole lease and no two threads write the same word. A bitmap over the gathered side is
/// one bit per gathered row however many driving rows there are, which is what makes merging
/// cheap: a semi join over six million driving rows merges the same eight kilobytes per instance
/// as one over six.
#[derive(Debug)]
pub(crate) struct Marking<'a> {
    /// The table, the equalities, the residual and the gathered rows, all of them already written.
    probe: Probe<'a>,
    /// What this operator produces, which is the gathered side and nothing beside it.
    schema: Schema,
    /// One bit per gathered row, set where some driving row matched it.
    marked: Mutex<Vec<u64>>,
    /// The one comparison left over after the equalities, when it is one [`Extents`] answers.
    spread: Option<Spread>,
    /// The smallest and the largest driving value per key, made on the first driving chunk.
    ///
    /// Nothing inside when the gathered column did not read as integers or the reservation said
    /// no, and the join then answers the residual pair by pair as it would without this.
    extents: OnceLock<Option<Extents>>,
    /// What the answer is charged, held for as long as it is readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// One instance's share of a marking join.
#[derive(Debug)]
pub(crate) struct Marks {
    /// The driving chunk's key columns, evaluated once when the chunk arrived.
    keys: Vec<Vector>,
    /// The driving chunk's slot per row, looked up once when the chunk arrived.
    slots: Vec<usize>,
    /// The buffers the lookup walks a chunk with, held here so that a chunk costs no allocation.
    scratch: Scratch,
    /// The gathered rows the current driving row matches, refilled per row from its chain.
    chain: Vec<u32>,
    /// The candidates a residual condition kept, when there is a residual condition.
    cand: Candidates,
    /// This instance's bits, one per gathered row, merged into the operator's in `combine`.
    ///
    /// Empty until the first chunk, because how many bits there are is how many rows the gathered
    /// side has and [`Sink::local`] cannot fail and so cannot read the table.
    bits: Vec<u64>,
    /// The driving column [`Extents::widen`] reads, a chunk at a time.
    block: Vec<i64>,
}

impl<'a> Marking<'a> {
    /// The marking join for this join, or nothing when this is not one a lookup answers.
    ///
    /// `left` is the schema of the side whose rows arrive here and `right` is the side the
    /// pipeline before this one gathered, exactly as [`Probe::new`] takes them. Unlike there,
    /// `kind` is the plan's own: turning a semi join around does not make it another kind, it
    /// makes it the same kind answered from the other end, which is why no new
    /// [`JoinKind`] had to be invented for this.
    pub(crate) fn new(
        plan: &'a Plan,
        left: &Schema,
        right: &Gathered<'_>,
        kind: JoinKind,
        conditions: Slice,
        cancel: &Cancel,
        memory: &Memory,
    ) -> Option<(Self, Buffered)> {
        if !matches!(kind, JoinKind::Semi | JoinKind::Anti) || !right.swapped {
            return None;
        }
        let probe = Probe::new(plan, left, right, kind, conditions, cancel, memory)?;
        let spread =
            Spread::of(plan, &probe.equalities.residual, &probe.combined, probe.left_width);
        let out = Buffered::new();
        let marking = Self {
            probe,
            schema: right.schema.clone(),
            marked: Mutex::new(Vec::new()),
            spread,
            extents: OnceLock::new(),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Some((marking, out))
    }

    /// Applies the session semantics to the key expressions.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.probe = self.probe.in_session(session);
        self
    }

    /// Reports the build side, which the probe inside this one does. See [`Probe::watched`].
    #[must_use]
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.probe = self.probe.watched(counters);
        self
    }

    /// What this operator produces, which is the gathered side's columns and nothing else.
    ///
    /// The gathered side is the plan's left input here, because that is what turning the join
    /// around means, and a semi or an anti join produces the plan's left input's columns. So there
    /// is no column order to put back and [`Probe::swapped`] has nothing to do in this operator.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The key this join can hand to the scan under its driving side.
    ///
    /// Both kinds, where [`Probe::sideways`] takes only the two that drop a driving row with no
    /// match. The rule there is about what a driving row with no match produces and here a driving
    /// row produces nothing at all: all it can do is set a bit, and a row whose key is outside the
    /// gathered side's range sets no bit whether it is read or not. So an anti join is on the list
    /// here, and it is the one that wants it most, since the side it drives is the larger one by
    /// construction.
    pub(crate) fn sideways(&self) -> Vec<(ExprRef, ColumnBinding)> {
        self.probe.keyed_sideways()
    }

    /// The ranges per key, made the first time a driving chunk asks, when this join has a spread.
    ///
    /// A reservation that says no is the same as a column that does not read as integers: the
    /// join answers pair by pair, which costs time rather than memory.
    fn extents(&self, built: &Built) -> Option<&Extents> {
        let spread = self.spread?;
        self.extents
            .get_or_init(|| {
                let column = built.rows.column(spread.gathered)?;
                let extents = Extents::new(built.index.slot_count(), column)?;
                let mut held = self.held.lock().ok()?;
                held.grow(extents.footprint()).ok()?;
                Some(extents)
            })
            .as_ref()
    }
}

impl Sink for Marking<'_> {
    type Local = Marks;

    fn local(&self) -> Marks {
        Marks {
            keys: Vec::new(),
            slots: Vec::new(),
            scratch: Scratch::default(),
            chain: Vec::new(),
            cand: Candidates::default(),
            bits: Vec::new(),
            block: Vec::new(),
        }
    }

    /// Builds the table here, where the whole lease is free, rather than inside an instance.
    ///
    /// The same reason [`Stream::prepare`] gives. The table is smaller than a probe's by the
    /// argument in this operator's documentation, but one thread building it while the rest of the
    /// lease sleeps on a lock is the same shape of waste whatever its size.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        self.probe.built_with(threads)?;
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Marks) -> Result<Progress> {
        let built = self.probe.built()?;
        let rows = built.rows.rows();
        if rows == 0 || chunk.is_empty() {
            return Ok(Progress::More);
        }
        if local.bits.is_empty() {
            local.bits = vec![0; rows.div_ceil(u64::BITS as usize)];
        }
        // Once per driving chunk rather than once per driving row, which is what keeps the driving
        // side on its batch interface. Nothing at all against an empty table, for the reason
        // [`Probing::keys`] gives: every lookup misses, so the keys would be evaluated to be
        // thrown away and a key expression that raises would raise where no pair existed.
        local.keys = if built.index.is_empty() {
            Vec::new()
        } else {
            evaluate_all_in_time_zone(
                self.probe.plan,
                &self.probe.equalities.left,
                &self.probe.left_schema,
                chunk,
                self.probe.time_zone,
            )?
        };
        local.slots.clear();
        if !local.keys.is_empty() {
            built.index.slots(
                &local.keys,
                chunk.len(),
                &self.probe.equalities.null_is_a_value,
                &mut local.scratch,
                &mut local.slots,
            );
        }
        // The residual answered by two numbers per key rather than pair by pair, see [`Extents`].
        if let Some(extents) = self.extents(&built) {
            let column = chunk.column(self.spread.map_or(0, |spread| spread.driving))?;
            if extents.widen(column, &local.slots, &mut local.block)? {
                return Ok(Progress::More);
            }
        }
        // What a residual answered about the chunk before this one says nothing about this one,
        // and the row numbers it is held under would be read as if it did.
        local.cand.forget();
        let residual = self.probe.residual();
        let Marks { slots, chain, cand, bits, .. } = local;
        // No check in here. It is one chain walk per row over a driving chunk of at most
        // [`VECTOR_SIZE`] rows, and the wrapper checks between chunks.
        for row in 0..chunk.len() {
            let found: &[u32] = if residual.exprs.is_empty() {
                let slot = slots.get(row).copied().unwrap_or(MISS);
                built.index.matches(slot, chain);
                chain
            } else {
                if !cand.holds(row) {
                    cand.from = row;
                    cand.fill(&residual, chunk, &built, slots)?;
                }
                cand.of(row)
            };
            for &at in found {
                let at = at as usize;
                if let Some(word) = bits.get_mut(at / u64::BITS as usize) {
                    *word |= 1 << (at % u64::BITS as usize);
                }
            }
        }
        Ok(Progress::More)
    }

    fn combine(&self, local: Marks) -> Result<()> {
        if local.bits.is_empty() {
            return Ok(());
        }
        let mut marked = self.marked.lock().map_err(poisoned)?;
        if marked.is_empty() {
            *marked = local.bits;
            return Ok(());
        }
        for (word, one) in marked.iter_mut().zip(local.bits) {
            *word |= one;
        }
        Ok(())
    }

    /// The gathered rows the bits chose, in the order the gathered side holds them.
    ///
    /// A chunk at a time and gathered at positions, which is [`Build::gather`] doing exactly what
    /// it does for a probe. The order does not depend on which instance marked which row, which is
    /// the promise [`Sink::parallel`] asks for.
    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let built = self.probe.built_with(threads)?;
        let mut marked = std::mem::take(&mut *self.marked.lock().map_err(poisoned)?);
        if let (Some(spread), Some(Some(extents))) = (self.spread, self.extents.get()) {
            marked.resize(built.rows.rows().div_ceil(u64::BITS as usize), 0);
            extents.mark(spread, &built.index, &mut marked);
        }
        // A semi join keeps the rows something matched and an anti join keeps the rest. That one
        // comparison is the whole difference between the two kinds here.
        let wanted = self.probe.kind == JoinKind::Semi;
        let mut chunks = Vec::new();
        let mut at: Vec<u32> = Vec::with_capacity(VECTOR_SIZE);
        let mut charged = self.held.lock().map_err(poisoned)?;
        for row in 0..built.rows.rows() {
            let bit = marked
                .get(row / u64::BITS as usize)
                .is_some_and(|word| word >> (row % u64::BITS as usize) & 1 == 1);
            if bit != wanted {
                continue;
            }
            at.push(u32::try_from(row).map_err(|_| unaddressable())?);
            if at.len() == VECTOR_SIZE {
                let chunk = built.rows.chunk(&at)?;
                charged.grow(chunk.footprint() as u64)?;
                chunks.push(chunk);
                at.clear();
            }
        }
        if !at.is_empty() {
            let chunk = built.rows.chunk(&at)?;
            charged.grow(chunk.footprint() as u64)?;
            chunks.push(chunk);
        }
        self.out.fill(chunks)
    }
}

/// An outer join that gathered the side it keeps.
///
/// A join that keeps one side's rows whatever they match cannot gather that side and stay a
/// stream, because a gathered row nothing has matched yet may still be matched, so nothing can be
/// said about it until the last driving row has been through. That is why [`streamed`] leaves
/// `RIGHT` and `FULL` off its list, and why `rudb_opt`'s `sides` pass used to refuse to gather an
/// outer join's kept side at all: gathering it handed the join to [`Join`], which pairs rows one
/// at a time and runs on one thread.
///
/// Refusing it costs the parallelism of the whole pipeline. TPC-H q13 is `customer LEFT JOIN
/// orders`, the kept side is the 150,000 row `customer` and the other side is the 1.5 million row
/// `orders`, so keeping the kept side streaming means the pipeline's driving scan is the small one
/// and its degree is what 150,000 rows are worth, which on this machine is two of ten threads. The
/// 1.5 million probes then run on those two threads. DuckDB answers the same query the other way
/// round, gathers `customer` and probes with `orders`, and gets the whole machine.
///
/// So this one turns the join around, and the observation that makes it possible is that the part
/// of the answer which has to wait is small. A driving row that matches produces its pairs the
/// moment it arrives, exactly as [`Probe`] would; what waits is the padded row owed to a gathered
/// row nothing matched, and there are at most as many of those as the gathered side has rows. On
/// q13 that is 1.5 million pairs that stream and about fifty thousand padded rows that do not.
/// [`Stream::drain`] is where the second half goes, so this is a stream with a tail rather than a
/// sink, and the answer is never materialised.
///
/// It holds a [`Probe`] and calls into it for the pairs, which is why the table, the lookup, the
/// equalities and the residual are written once. The kind that probe is given is not this join's:
/// what a `RIGHT` join should do with a driving row that matches nothing is drop it, which is
/// `INNER`, and what a `FULL` join should do with one is pad it, which is `LEFT`. Those are the
/// two kinds this operator is built for and they are the two substitutions.
///
/// # The bits
///
/// One per gathered row, shared by every instance and set with a relaxed `fetch_or` rather than
/// kept per instance and merged. [`Marking`] does it the other way because it is a sink and
/// [`Sink::combine`] is a merge point that already exists. There is no such point here: a drain
/// runs after the last instance has finished and there is no hook between the two, so the bits
/// have to be right the moment the instances stop. A word covers sixty four gathered rows, the
/// write is on the match path only, and the ordering can be relaxed because joining the threads is
/// what makes them visible to the drain.
#[derive(Debug)]
pub(crate) struct Padding<'a> {
    /// The pairs, and everything it takes to find them.
    probe: Probe<'a>,
    /// One bit per gathered row, set where some driving row matched it.
    ///
    /// Filled in [`Stream::prepare`], because how many bits there are is how many rows the
    /// gathered side has and that is not known until the table is built.
    marked: OnceLock<Vec<AtomicU64>>,
}

/// One instance's share of a padding join.
#[derive(Debug)]
pub(crate) struct Padded {
    /// The probe's own state, since the pairs are its work.
    probing: Probing,
}

impl<'a> Padding<'a> {
    /// The operator for this join, or nothing when this is not one it answers.
    ///
    /// `kind` is the join as this operator sees it, which is with the gathered side on the right,
    /// so `RIGHT` here means the gathered side is the kept one. That is the case worth turning
    /// around and the only one this is for.
    pub(crate) fn new(
        plan: &'a Plan,
        left: &Schema,
        right: &Gathered<'_>,
        kind: JoinKind,
        conditions: Slice,
        cancel: &Cancel,
        memory: &Memory,
    ) -> Option<Self> {
        // What the probe should do with a driving row that matches nothing, which is the whole of
        // what this operator delegates. A right join drops it and a full join pads it.
        let pairing = match kind {
            JoinKind::Right => JoinKind::Inner,
            JoinKind::Full => JoinKind::Left,
            _ => return None,
        };
        let probe = Probe::new(plan, left, right, pairing, conditions, cancel, memory)?;
        Some(Self { probe, marked: OnceLock::new() })
    }

    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.probe = self.probe.in_session(session);
        self
    }

    /// Reports the build side, which the probe inside this one does. See [`Probe::watched`].
    #[must_use]
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.probe = self.probe.watched(counters);
        self
    }

    pub(crate) fn schema(&self) -> &Schema {
        self.probe.schema()
    }

    /// The key this join can hand to the scan under its driving side.
    ///
    /// Nothing at all, and this is the one place where turning a join around costs something. A
    /// runtime filter over the driving side drops rows the gathered side has no key for, and a
    /// driving row this operator drops is one that marked no bit, so for the pairs it would be
    /// sound. It is not sound for the padding: the filter would be built from the gathered side
    /// and the rows it removes are exactly the ones that were going to match nothing, which is the
    /// half of a full join's answer that comes out of the drain. A right join could have it and
    /// does not, because the two kinds share this operator and a filter that is right for one of
    /// them and wrong for the other is worse than none.
    pub(crate) fn sideways(&self) -> Vec<(ExprRef, ColumnBinding)> {
        Vec::new()
    }

    /// Note that every one of these gathered rows has now been matched.
    fn mark(&self, at: &[u32]) {
        let Some(marked) = self.marked.get() else {
            return;
        };
        for &row in at {
            if row == PAD {
                continue;
            }
            let row = row as usize;
            if let Some(word) = marked.get(row / u64::BITS as usize) {
                word.fetch_or(1 << (row % u64::BITS as usize), Ordering::Relaxed);
            }
        }
    }

    /// The chunk those gathered rows make, with nulls where the driving side would have been.
    fn padded(&self, built: &Built, at: &[u32]) -> Result<Chunk> {
        let mut columns: Vec<Vector> = self
            .probe
            .left_types
            .iter()
            .map(|ty| Vector::constant(ty.clone(), Value::Null, at.len()))
            .collect();
        columns.extend(built.rows.gather(at)?);
        if self.probe.swapped {
            // The same rotation [`Probe::push`] ends with and for the same reason, since these
            // rows go to whatever that operator's rows go to and have to be laid out alike.
            columns.rotate_left(self.probe.left_width);
        }
        Chunk::with_rows(columns, at.len())
    }
}

impl Stream for Padding<'_> {
    type Local = Padded;

    fn local(&self) -> Padded {
        Padded { probing: Stream::local(&self.probe) }
    }

    /// Builds the table on the whole lease, and makes the bits now that their number is known.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        let built = self.probe.built_with(threads)?;
        let words = built.rows.rows().div_ceil(u64::BITS as usize);
        let _ = self.marked.set((0..words).map(|_| AtomicU64::new(0)).collect());
        Ok(())
    }

    fn drains(&self) -> bool {
        true
    }

    /// The gathered rows nothing matched, padded, in the order the gathered side holds them.
    fn drain(&self, out: &mut dyn FnMut(&mut Chunk) -> Result<Progress>) -> Result<()> {
        let built = self.probe.built()?;
        let rows = built.rows.rows();
        let empty = Vec::new();
        let marked = self.marked.get().unwrap_or(&empty);
        let mut at: Vec<u32> = Vec::with_capacity(VECTOR_SIZE);
        for (word, bits) in marked.iter().enumerate() {
            let bits = bits.load(Ordering::Relaxed);
            // A word that is all ones is sixty four gathered rows every one of which matched, and
            // on a join that mostly matches it is most of the words. The check is one comparison
            // for the sixty four rows it skips.
            if bits == u64::MAX {
                continue;
            }
            let first = word * u64::BITS as usize;
            for bit in 0..u64::BITS as usize {
                let row = first + bit;
                if row >= rows {
                    break;
                }
                if bits >> bit & 1 == 1 {
                    continue;
                }
                at.push(u32::try_from(row).map_err(|_| unaddressable())?);
                if at.len() == VECTOR_SIZE {
                    let mut chunk = self.padded(&built, &at)?;
                    at.clear();
                    if out(&mut chunk)? == Progress::Done {
                        return Ok(());
                    }
                }
            }
        }
        if !at.is_empty() {
            let mut chunk = self.padded(&built, &at)?;
            out(&mut chunk)?;
        }
        Ok(())
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Padded) -> Result<Progress> {
        let progress = self.probe.push(chunk, &mut local.probing)?;
        // After the probe rather than inside it, because what it wrote is exactly the answer to
        // the question this operator is asking: one entry per output row saying which gathered row
        // it read from, with [`PAD`] where it read from none.
        self.mark(&local.probing.right_at);
        Ok(progress)
    }
}

/// What a join says when its gathered side holds more rows than a position can name.
fn unaddressable() -> Error {
    Error::internal("a join gathered more rows than it can address")
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
    /// Which of those columns the conjuncts actually read, in the same order.
    wanted: &'a [bool],
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

    /// Whether each of a list of pairs holds, for pairs that came from more than one driving row.
    ///
    /// The pairs are two lists of the same length read side by side: `driving` says which row of
    /// `left` each pair uses and `gathered` says which row of `rows`. `into` comes back with one
    /// answer per pair, in the same order.
    ///
    /// This is [`Residual::keep`] with the driving row no longer fixed, and that is the whole point
    /// of it. Fixing the driving row means the vectors are as long as one key's candidate list,
    /// which on a join between two large tables is a handful of rows, and then the cost of walking
    /// an expression tree and allocating a vector per node is paid once per handful. Here the batch
    /// is full whatever the candidate lists look like. What it costs in exchange is a gather of the
    /// driving columns, where the fixed version repeated one row across the batch as constants.
    /// That is a real cost and it is per pair rather than per driving row, but it is one pass of the
    /// same kernel the answer itself is built with, and on TPC-H q21 the two joins that carry a
    /// residual average about four candidates per driving row, so the fixed costs it removes are
    /// paid two hundred thousand times and the gather it adds reads the same rows the evaluation
    /// was going to read anyway.
    fn keeps(
        &self,
        left: &Chunk,
        rows: &Build,
        driving: &[u32],
        gathered: &[u32],
        into: &mut Vec<bool>,
    ) -> Result<()> {
        into.clear();
        if self.exprs.is_empty() {
            // Nothing left to say keeps every pair, and saying so here rather than at the call site
            // is what lets a caller treat a join with a residual and one without it the same way.
            into.resize(gathered.len(), true);
            return Ok(());
        }
        into.reserve(gathered.len());
        let mut at = 0;
        while at < gathered.len() {
            // A vector at a time, which is the size the evaluator is written for. The batch above
            // this is larger so that a batch is full, and this is what it is broken back down into.
            let end = (at + VECTOR_SIZE).min(gathered.len());
            // Where the mask stops being about the driving side, which is the driving schema's
            // width and not the chunk's, because the mask was built against the two schemas.
            let width = self.left_types.len();
            let mut columns: Vec<Vector> = left
                .columns()
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    if self.wanted.get(index).copied().unwrap_or(true) {
                        column.gather(&driving[at..end])
                    } else {
                        Ok(stood_in_for(column, end - at))
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            columns.extend(
                rows.gather_wanted(
                    &gathered[at..end],
                    self.wanted.get(width..).unwrap_or_default(),
                )?,
            );
            // Driving columns and then gathered ones, which is the order this operator holds a pair
            // in and so the order `self.combined` resolves a conjunct against. See [`widen`], which
            // builds the same layout out of a single row.
            let combined = Chunk::with_rows(columns, end - at)?;
            let flags = evaluate_all_in_time_zone(
                self.plan,
                self.exprs,
                self.combined,
                &combined,
                self.time_zone,
            )?;
            let merged = combine(Connective::And, &flags)?;
            // A conjunction of comparisons over a chunk is a flat boolean, so the answer is a run
            // of bits beside a validity and reading it is a pass over the two. What this wants in
            // the end is the selection that 2c (#57) threads, which would leave the flags where
            // they are rather than copying them into a list of `bool`.
            let rows = end - at;
            if let Some(Data::Bool(flags)) = merged.data() {
                let valid = merged.validity();
                let flags = flags.as_slice();
                into.extend(
                    (0..rows)
                        .map(|row| valid.is_valid(row) && flags.get(row).copied().unwrap_or(false)),
                );
            } else {
                // row at a time: whatever else a conjunct's result arrives as, a constant or a
                // dictionary or a run, is read through the one accessor every form answers. The
                // flat case above is the one this operator actually produces.
                for row in 0..rows {
                    into.push(is_true(&merged.value_at(row)));
                }
            }
            at = end;
        }
        Ok(())
    }
}

/// A column of the right type and length that nothing is going to read.
///
/// What goes where a column the residual does not name would have gone, so that the columns after
/// it are still where the conjuncts expect to find them. A constant is one value however many rows
/// it stands for, so this costs nothing per pair, which is the whole of why it is here.
fn stood_in_for(column: &Vector, rows: usize) -> Vector {
    Vector::constant(column.logical_type().clone(), Value::Null, rows)
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
pub(crate) fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    match *plan.expr(expr) {
        Expr::Column(binding) => found(binding),
        Expr::Constant(_) | Expr::LambdaParam(_) => {}
        Expr::Cast { input, .. } => columns(plan, input, found),
        Expr::Lambda { body, .. } => columns(plan, body, found),
        Expr::Compare { left, right, .. } => {
            columns(plan, left, found);
            columns(plan, right, found);
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            for &child in plan.expr_list(children) {
                columns(plan, child, found);
            }
        }
        Expr::Aggregate { args, filter, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
        }
        Expr::Window { args, filter, order, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
            for key in plan.sort_key_list(order) {
                columns(plan, key.expr, found);
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
/// Every part of what this costs is a pass over a column rather than a walk over a row: the key
/// expressions are evaluated through the vectorized evaluator the rest of this file uses, the hash
/// is one pass per key column with the column's type matched on once, and the probe walks the rows
/// of a batch together so the cache misses on a table larger than the cache are outstanding at the
/// same time. What this replaced built a `Vec<Value>` per gathered row and hashed it a tagged value
/// at a time. See [`Lookup`] for the rest of the argument.
///
/// The keys are evaluated a chunk at a time and then laid end to end, because the table is built in
/// partitions and a partition's rows are scattered through the side. Laying them out is a thread
/// per column and the partitions are a thread each, so the only part of this left on one thread is
/// the hash and the null test.
///
/// What is charged is the table, once it is built rather than as it builds. The partitions run at
/// the same time and a reservation taken in the middle of one of them would be a lock the others
/// wait on, which is the thing this whole arrangement exists to remove.
fn lookup(
    keying: Keying<'_>,
    chunks: &[Chunk],
    cancel: &Cancel,
    threads: &Lease<'_>,
    scratch: &mut Reservation,
) -> Result<Lookup> {
    let Keying { plan, exprs, schema, nulls, time_zone } = keying;
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let mut keyed: Vec<Chunk> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        // Once per chunk rather than once per row. A build over a side nobody bounded is the one
        // part of this operator that can run long without producing anything, and a check every two
        // thousand rows is the same granularity the rest of the operator uses.
        cancel.check()?;
        let columns = evaluate_all_in_time_zone(plan, exprs, schema, chunk, time_zone)?;
        keyed.push(Chunk::with_rows(columns, chunk.len())?);
    }
    let Some(types) = keyed.first().map(Chunk::types) else {
        return Lookup::build(&[], 0, nulls, threads, cancel);
    };
    let keys = laid_out(&types, &keyed, threads)?;
    drop(keyed);
    let lookup = Lookup::build(&keys, rows, nulls, threads, cancel)?;
    scratch.grow(lookup.footprint())?;
    Ok(lookup)
}

/// The key columns out of the side already laid end to end, when every key is one of its columns.
///
/// Every equality in TPC-H is a column against a column, and [`lookup`] evaluated each key over each
/// chunk on one thread and then laid the answers end to end, which for a bare column is the same
/// copy [`Build::new`] has just made of it. On q9 that was the 800,000 rows of `partsupp` copied a
/// second time before the table could start. A key that is anything else, a cast or an expression,
/// still goes through [`lookup`], and so does a side with no rows, which has no columns to take.
fn laid_keys(keying: Keying<'_>, rows: &Build) -> Option<Vec<Vector>> {
    if rows.rows() == 0 {
        return None;
    }
    keying
        .exprs
        .iter()
        .map(|&expr| match *keying.plan.expr(expr) {
            Expr::Column(binding) => rows.column(keying.schema.position_of(binding)?).cloned(),
            _ => None,
        })
        .collect()
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
    use rudb_vector::{Data, Validity, Vector};

    use super::{
        Buffered, Chunk, CrossProduct, Gathered, Join, Marking, Padding, Probe, Progress, Schema,
        Side, Sink, Stream, VECTOR_SIZE, equalities, side_of,
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

    /// A column of integers with a null wherever the value is missing.
    fn some_column(values: &[Option<i32>]) -> Vector {
        let held: Vec<i32> = values.iter().map(|value| value.unwrap_or_default()).collect();
        let valid: Vec<bool> = values.iter().map(Option::is_some).collect();
        Vector::flat(LogicalType::Integer, Data::Int32(held.into()))
            .expect("integers are an i32 layout")
            .with_validity(Validity::from_run(&valid))
    }

    fn some_chunk(values: &[Option<i32>]) -> Chunk {
        Chunk::with_rows(vec![some_column(values)], values.len()).expect("one column is one length")
    }

    /// A gathered side shaped the way the unnesting writes one for a mark join: the key it is
    /// looked up by, and beside it the column the marker is put in.
    fn marked_schema(table: u32) -> Schema {
        Schema::numbered(
            vec![Field::new("k", LogicalType::Integer), Field::new("mark", LogicalType::Boolean)],
            table,
        )
    }

    /// Rows of that side. The marker column is null in all of them, because what the join puts
    /// there is its own answer and nothing ever reads what the side held.
    fn marked_chunk(keys: &[Option<i32>]) -> Chunk {
        let mark = Vector::constant(LogicalType::Boolean, Value::Null, keys.len());
        Chunk::with_rows(vec![some_column(keys), mark], keys.len())
            .expect("two columns of one length")
    }

    /// Every driving row's marker out of the streaming path, for a mark join on one equality.
    ///
    /// The answer is the driving column, then the gathered side's two, so the marker is column
    /// two. That is the shape [`Join`] produces for the same join and the shape the projection
    /// above it was built against.
    fn markers(gathered: &[Option<i32>], driving: &[Option<i32>]) -> Vec<Value> {
        let mut plan = Plan::new();
        let (left, right) = (schema("a", 0), marked_schema(1));
        let conditions = {
            let one = column(&mut plan, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        if !gathered.is_empty() {
            keep.sink(&marked_chunk(gathered), &mut local).expect("the gathered rows");
        }
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let probe = Probe::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: rows, marker: Some(1), swapped: false },
            JoinKind::Mark,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to mark on");

        probed(&probe, &some_chunk(driving), 3).into_iter().map(|row| row[2].clone()).collect()
    }

    /// The whole of the three valued rule, in the case where all three answers turn up. The
    /// gathered side holds a null key, so a driving row that missed cannot be told from one whose
    /// comparison was unknown, and both come out null.
    #[test]
    fn a_mark_join_over_a_gathered_side_with_a_null_key_marks_every_miss_null() {
        assert_eq!(
            markers(&[Some(2), None], &[Some(2), Some(3), None]),
            [Value::Boolean(true), Value::Null, Value::Null]
        );
    }

    /// The same side without the null in it, where a miss is a miss. A driving row with a null key
    /// is still unknown, because that half of the rule is about the row rather than the side.
    #[test]
    fn a_mark_join_over_a_side_with_no_null_key_marks_a_miss_false() {
        assert_eq!(
            markers(&[Some(2)], &[Some(2), Some(3), None]),
            [Value::Boolean(true), Value::Boolean(false), Value::Null]
        );
    }

    /// A gathered side with no rows in it has no pairs at all, so nothing is unknown and even the
    /// driving row whose own key is null comes out false.
    #[test]
    fn a_mark_join_over_an_empty_gathered_side_marks_everything_false() {
        assert_eq!(markers(&[], &[Some(2), None]), [Value::Boolean(false), Value::Boolean(false)]);
    }

    /// The side the one above has to be told apart from. Both reach the probe with an empty table,
    /// because a null key is not stored under `=`, and they answer opposite things.
    #[test]
    fn a_mark_join_over_a_side_of_nothing_but_nulls_marks_everything_null() {
        assert_eq!(markers(&[None, None], &[Some(2), None]), [Value::Null, Value::Null]);
    }

    /// Two equalities are not the rule a lookup answers, for the reason [`Join::marks`] gives, so
    /// the probe hands the join back and the row major operator decides it.
    #[test]
    fn a_mark_join_on_two_equalities_is_not_streamed() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(0), pair_schema(1));
        let conditions = {
            let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            let first = equal(&mut plan, one, other);
            let above = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let below = column_at(&mut plan, 1, 1, LogicalType::Integer);
            let second = equal(&mut plan, above, below);
            plan.add_expr_list(&[first, second])
        };
        let memory = Memory::unlimited();
        let (_keep, rows) = gathered(&memory, &[]);

        assert!(
            Probe::new(
                &plan,
                &left,
                &Gathered { schema: &right, chunks: rows, marker: Some(1), swapped: false },
                JoinKind::Mark,
                conditions,
                &Cancel::new(),
                &memory,
            )
            .is_none()
        );
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

    /// A residual is answered a batch of pairs at a time and a batch that fills up in the middle of
    /// a driving chunk has to pick up from the row it stopped at. See [`RESIDUAL_BATCH`], which is
    /// what decides where that happens, and [`Candidates`] for what is held across it.
    #[test]
    fn a_residual_over_more_pairs_than_fit_in_a_batch_answers_the_same() {
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

        // One key on both sides, so every driving row is a candidate for every gathered row. Two
        // hundred of each is forty thousand pairs, which is more than one batch holds, and the
        // answer crosses the boundary in the middle of a driving row rather than between two.
        let side: Vec<(i32, i32)> = (0..200).map(|at| (7, at)).collect();
        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&pair_chunk(&side), &mut local).expect("the gathered rows");
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

        // A driving row pairs with the gathered rows numbered below it, so the whole answer is the
        // number of ordered pairs of two hundred things.
        let answer = probed(&probe, &pair_chunk(&side), 4);
        assert_eq!(answer.len(), 200 * 199 / 2);
        assert!(
            answer.iter().all(|row| match (&row[1], &row[3]) {
                (Value::Integer(driving), Value::Integer(gathered)) => driving > gathered,
                _ => false,
            }),
            "every pair the residual kept is one it should have"
        );
    }

    /// A side carrying a column the residual never names still answers that column.
    ///
    /// The chunk a residual is evaluated over is gathered at the pair list, and a column no
    /// conjunct reads is stood in for rather than gathered, so that the column numbers the
    /// conjuncts use still land where they did. Two ways that can go wrong and this catches both:
    /// a column the residual does read gets stood in for, and then the comparison is against null
    /// and the answer is empty, or the standing in reaches the output chunk, and then the payload
    /// comes back null. The answer here has the payloads in it and the right pairs.
    #[test]
    fn a_column_the_residual_does_not_read_is_still_in_the_answer_with_its_own_values() {
        let wide = |table: u32| {
            Schema::numbered(
                vec![
                    Field::new("k", LogicalType::Integer),
                    Field::new("g", LogicalType::Integer),
                    Field::new("p", LogicalType::Integer),
                ],
                table,
            )
        };
        let chunk = |rows: &[(i32, i32, i32)]| {
            let each = |pick: fn(&(i32, i32, i32)) -> i32| {
                Vector::flat(
                    LogicalType::Integer,
                    Data::Int32(rows.iter().map(pick).collect::<Vec<i32>>().into()),
                )
                .expect("integers are an i32 layout")
            };
            Chunk::new(vec![each(|row| row.0), each(|row| row.1), each(|row| row.2)])
                .expect("three columns of one length")
        };

        let mut plan = Plan::new();
        let (left, right) = (wide(0), wide(1));
        let key = {
            let one = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 0, LogicalType::Integer);
            equal(&mut plan, one, other)
        };
        // Only position 1 of each side, so the payload at position 2 is read by nothing.
        let beside = {
            let one = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let other = column_at(&mut plan, 1, 1, LogicalType::Integer);
            greater(&mut plan, one, other)
        };
        let conditions = plan.add_expr_list(&[key, beside]);

        let memory = Memory::unlimited();
        let (keep, rows) = Keep::new(&memory);
        let mut local = keep.local();
        keep.sink(&chunk(&[(2, 5, 200), (2, 50, 201), (3, 5, 202)]), &mut local)
            .expect("the gathered rows");
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
        assert_eq!(probe.wanted, [false, true, false, false, true, false]);

        let number = Value::Integer;
        assert_eq!(
            probed(&probe, &chunk(&[(1, 10, 100), (2, 10, 101), (3, 10, 102)]), 6),
            [
                vec![number(2), number(10), number(101), number(2), number(5), number(200)],
                vec![number(3), number(10), number(102), number(3), number(5), number(202)],
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

        probe.sideways().into_iter().next()
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

    /// The subject side of a turned around join, gathered the way the pipeline before it would.
    fn subject(rows: &[Option<i32>], memory: &Memory) -> Buffered {
        let (keep, kept) = Keep::new(memory);
        let mut local = keep.local();
        if !rows.is_empty() {
            keep.sink(&some_chunk(rows), &mut local).expect("the gathered rows");
        }
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        kept
    }

    /// Every row a marking join answers with, given the subject it gathered and what drives it.
    ///
    /// `driving` is a chunk per instance rather than a chunk per call, so a test that hands over
    /// two of them is a test of two instances marking the same side and their bits being merged.
    /// `kind` is the plan's own, because turning a join around does not make it another kind.
    fn marking(
        kind: JoinKind,
        subject_rows: &[Option<i32>],
        driving: &[&[Option<i32>]],
    ) -> Vec<Value> {
        let mut plan = Plan::new();
        // The driving side is this operator's left and the gathered subject is its right, which is
        // what `swapped` says: the subject is the plan's left input.
        let (left, right) = (schema("b", 1), schema("a", 0));
        let conditions = {
            let one = column(&mut plan, 1, LogicalType::Integer);
            let other = column(&mut plan, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        let kept = subject(subject_rows, &memory);
        let (mark, out) = Marking::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
            kind,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to mark on");
        for rows in driving {
            let mut local = mark.local();
            mark.sink(&some_chunk(rows), &mut local).expect("a driving chunk");
            mark.combine(local).expect("one instance");
        }
        mark.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");
        answered_rows(&out)
    }

    /// The one column of every chunk a sink left in its buffer.
    fn answered_rows(out: &Buffered) -> Vec<Value> {
        let reader = out.reader();
        let mut rows = Vec::new();
        for at in 0..reader.len().expect("the chunks") {
            let chunk = reader.at(at).expect("the chunk").expect("a chunk that was counted");
            rows.extend((0..chunk.len()).map(|row| chunk.value_at(row, 0)));
        }
        rows
    }

    /// The point of the operator. A semi join produces the rows of the side it gathered, in the
    /// order that side holds them, and the side it gathered is the subject.
    #[test]
    fn a_marking_semi_join_answers_with_the_gathered_rows_something_matched() {
        assert_eq!(
            marking(JoinKind::Semi, &[Some(1), Some(2), Some(3)], &[&[Some(3), Some(1)]]),
            [Value::Integer(1), Value::Integer(3)]
        );
    }

    /// The other half of the same bitmap, which is the whole difference between the two kinds.
    #[test]
    fn a_marking_anti_join_answers_with_the_gathered_rows_nothing_matched() {
        assert_eq!(
            marking(JoinKind::Anti, &[Some(1), Some(2), Some(3)], &[&[Some(3), Some(1)]]),
            [Value::Integer(2)]
        );
    }

    /// A subject row matched by many driving rows is still one row of the answer. That is what a
    /// semi join means and it is what a bit rather than a counter gives for free.
    #[test]
    fn a_marking_semi_join_answers_a_subject_row_once_however_often_it_matched() {
        assert_eq!(
            marking(JoinKind::Semi, &[Some(1), Some(2)], &[&[Some(2), Some(2), Some(2)]]),
            [Value::Integer(2)]
        );
    }

    /// Two instances, each with its own bits, merged in `combine`. A subject row is in the answer
    /// when either of them marked it, which is what makes the driving side safe to run in
    /// parallel.
    #[test]
    fn a_marking_join_puts_the_bits_of_two_instances_together() {
        assert_eq!(
            marking(JoinKind::Semi, &[Some(1), Some(2), Some(3)], &[&[Some(1)], &[Some(3)]]),
            [Value::Integer(1), Value::Integer(3)]
        );
    }

    /// Nothing drives it at all, which is the case the bitmap is never sized for. A semi join over
    /// an empty other side keeps nothing and an anti join over one keeps everything.
    #[test]
    fn a_marking_join_over_a_driving_side_with_no_rows_marks_nothing() {
        assert_eq!(marking(JoinKind::Semi, &[Some(1), Some(2)], &[]), []);
        assert_eq!(
            marking(JoinKind::Anti, &[Some(1), Some(2)], &[]),
            [Value::Integer(1), Value::Integer(2)]
        );
    }

    /// The null rule, from the other end. Under `=` a null key matches nothing, so a subject row
    /// whose key is null is never marked however the other side is written, and it is the anti
    /// join that keeps it.
    #[test]
    fn a_marking_join_never_marks_a_subject_row_whose_key_is_null() {
        assert_eq!(
            marking(JoinKind::Semi, &[Some(1), None], &[&[Some(1), None]]),
            [Value::Integer(1)]
        );
        assert_eq!(marking(JoinKind::Anti, &[Some(1), None], &[&[Some(1), None]]), [Value::Null]);
    }

    /// A subject with no rows in it answers nothing whichever kind asks, and the table it builds
    /// is the empty one every driving row misses in.
    #[test]
    fn a_marking_join_over_an_empty_subject_answers_nothing() {
        assert_eq!(marking(JoinKind::Semi, &[], &[&[Some(1)]]), []);
        assert_eq!(marking(JoinKind::Anti, &[], &[&[Some(1)]]), []);
    }

    /// The condition the lookup did not answer, evaluated on the candidates the equality found and
    /// read back a driving row at a time. TPC-H q21's two turned around joins are written this
    /// way, `ON l.orderkey = o.orderkey AND l.suppkey <> o.suppkey`, so the residual is not a
    /// corner of this operator but the case it was written for.
    #[test]
    fn a_marking_join_marks_only_what_the_residual_kept() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(1), pair_schema(0));
        let conditions = {
            let one = column_at(&mut plan, 1, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            let driving_g = column_at(&mut plan, 1, 1, LogicalType::Integer);
            let subject_g = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let over = greater(&mut plan, driving_g, subject_g);
            plan.add_expr_list(&[key, over])
        };
        let memory = Memory::unlimited();
        let (keep, kept) = Keep::new(&memory);
        let mut local = keep.local();
        // Two subject rows on the same key, told apart only by the column the residual reads.
        keep.sink(&pair_chunk(&[(7, 1), (7, 9), (8, 1)]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let (mark, out) = Marking::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
            JoinKind::Semi,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("an equality beside a residual is still a lookup");
        let mut instance = mark.local();
        // Key 7 finds both subject rows and the residual keeps the one whose `g` is under 5. Key
        // 8 finds the third and the residual throws it away.
        mark.sink(&pair_chunk(&[(7, 5), (8, 0)]), &mut instance).expect("a driving chunk");
        mark.combine(instance).expect("the one instance");
        mark.finalize(&rudb_pipeline::Lease::alone()).expect("the answer");

        assert_eq!(answered_rows(&out), [Value::Integer(7)]);
    }

    /// An anti join is on the list a semi join is on, which [`Probe::sideways`] is not. A driving
    /// row here can only set a bit, and one whose key no gathered row holds sets none whether the
    /// scan reads it or not, so the scan may as well not read it.
    #[test]
    fn a_marking_join_offers_its_key_to_the_scan_under_either_kind() {
        for kind in [JoinKind::Semi, JoinKind::Anti] {
            let mut plan = Plan::new();
            let (left, right) = (schema("b", 1), schema("a", 0));
            let conditions = {
                let one = column(&mut plan, 1, LogicalType::Integer);
                let other = column(&mut plan, 0, LogicalType::Integer);
                let key = equal(&mut plan, one, other);
                plan.add_expr_list(&[key])
            };
            let memory = Memory::unlimited();
            let kept = subject(&[Some(1)], &memory);
            let (mark, _out) = Marking::new(
                &plan,
                &left,
                &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
                kind,
                conditions,
                &Cancel::new(),
                &memory,
            )
            .expect("one equality is enough to mark on");
            assert!(!mark.sideways().is_empty(), "a {} join has a key to offer", kind.keyword());
        }
    }

    /// The operator refuses the join it was not written for rather than answering it wrongly.
    /// Nothing in the executor asks it to, because only `sides` turns a join around and it turns
    /// around no other kind, but the refusal is what makes that a fact about one pass rather than
    /// a thing two places have to agree on.
    #[test]
    fn a_marking_join_refuses_a_kind_it_does_not_answer() {
        let mut plan = Plan::new();
        let (left, right) = (schema("b", 1), schema("a", 0));
        let conditions = {
            let one = column(&mut plan, 1, LogicalType::Integer);
            let other = column(&mut plan, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        for kind in [JoinKind::Inner, JoinKind::Left, JoinKind::Single, JoinKind::Mark] {
            let kept = subject(&[Some(1)], &memory);
            let made = Marking::new(
                &plan,
                &left,
                &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
                kind,
                conditions,
                &Cancel::new(),
                &memory,
            );
            assert!(made.is_none(), "a {} join is not a marking join", kind.keyword());
        }
    }

    /// What a padding join produced, split into the half that streamed and the half that waited.
    ///
    /// The split is the point of the operator, so a test that put the two back together again
    /// would be checking the answer and not the thing worth checking.
    #[derive(Debug)]
    struct Answered {
        /// The pairs, in the order the driving rows arrived.
        pairs: Vec<Vec<Value>>,
        /// The gathered rows nothing matched, padded, in the order the gathered side holds them.
        padded: Vec<Vec<Value>>,
        /// How many rows were in each chunk the drain produced.
        chunks: Vec<usize>,
    }

    /// Every row of one chunk, `width` columns wide.
    fn rows_of(chunk: &Chunk, width: usize) -> Vec<Vec<Value>> {
        (0..chunk.len())
            .map(|row| (0..width).map(|column| chunk.value_at(row, column)).collect())
            .collect()
    }

    /// Run a padding join over one gathered side and a driving chunk per instance.
    ///
    /// `driving` is a chunk per instance rather than a chunk per call, so a test that hands over
    /// two of them is a test of two instances setting bits in the one shared bitmap. `kind` is
    /// this operator's own, which is the join with the gathered side on the right, and the
    /// gathered side is the plan's left, which is the way round `sides` produces.
    fn padding(kind: JoinKind, gathered: &[Option<i32>], driving: &[&[Option<i32>]]) -> Answered {
        let mut plan = Plan::new();
        let (left, right) = (schema("b", 1), schema("a", 0));
        let conditions = {
            let one = column(&mut plan, 1, LogicalType::Integer);
            let other = column(&mut plan, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        let kept = gathered_side(gathered, &memory);
        let pad = Padding::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
            kind,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to pair on");
        answered(&pad, &driving.iter().map(|rows| some_chunk(rows)).collect::<Vec<Chunk>>(), 2)
    }

    /// The gathered side, kept the way the pipeline before it would, a vector at a time.
    ///
    /// Not [`subject`], which takes the lot in one chunk, because a gathered side long enough to
    /// make the drain produce more than one chunk is longer than a chunk can be.
    fn gathered_side(rows: &[Option<i32>], memory: &Memory) -> Buffered {
        let (keep, kept) = Keep::new(memory);
        let mut local = keep.local();
        for piece in rows.chunks(VECTOR_SIZE) {
            keep.sink(&some_chunk(piece), &mut local).expect("the gathered rows");
        }
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        kept
    }

    /// Push a chunk per instance through a padding join and then take what it owes.
    fn answered(pad: &Padding<'_>, driving: &[Chunk], width: usize) -> Answered {
        pad.prepare(&rudb_pipeline::Lease::alone()).expect("the table");
        let mut pairs = Vec::new();
        for chunk in driving {
            let mut local = pad.local();
            let mut chunk = chunk.clone();
            loop {
                let progress = pad.push(&mut chunk, &mut local).expect("a chunk");
                pairs.extend(rows_of(&chunk, width));
                if progress != Progress::Again {
                    break;
                }
                chunk = Chunk::empty(&[]);
            }
        }
        let mut padded = Vec::new();
        let mut chunks = Vec::new();
        pad.drain(&mut |chunk| {
            chunks.push(chunk.len());
            padded.extend(rows_of(chunk, width));
            Ok(Progress::More)
        })
        .expect("the gathered rows nothing matched");
        Answered { pairs, padded, chunks }
    }

    /// The point of the operator. The pairs come out as the driving rows arrive and the gathered
    /// row nothing matched comes out at the end, which is what lets the driving side run on every
    /// thread the machine has rather than on however many the gathered side is worth.
    #[test]
    fn a_padding_right_join_streams_the_pairs_and_pads_what_nothing_matched() {
        let answer = padding(JoinKind::Right, &[Some(1), Some(2), Some(3)], &[&[Some(3), Some(1)]]);
        assert_eq!(
            answer.pairs,
            [
                vec![Value::Integer(3), Value::Integer(3)],
                vec![Value::Integer(1), Value::Integer(1)]
            ]
        );
        assert_eq!(answer.padded, [vec![Value::Integer(2), Value::Null]]);
    }

    /// A full join is the same drain with the other half of the pairing. The driving row nothing
    /// matched is padded by the probe as it arrives, because that much is known then, and only
    /// the gathered row nothing matched has to wait.
    #[test]
    fn a_padding_full_join_pads_the_driving_side_as_it_goes_and_the_gathered_side_at_the_end() {
        let answer = padding(JoinKind::Full, &[Some(1), Some(2)], &[&[Some(2), Some(9)]]);
        assert_eq!(
            answer.pairs,
            [vec![Value::Integer(2), Value::Integer(2)], vec![Value::Null, Value::Integer(9)]]
        );
        assert_eq!(answer.padded, [vec![Value::Integer(1), Value::Null]]);
    }

    /// A gathered row matched many times is paired many times and padded none, which is what one
    /// bit per gathered row gives without anything having to count.
    #[test]
    fn a_padding_join_pads_a_gathered_row_no_times_however_often_it_matched() {
        let answer = padding(JoinKind::Right, &[Some(1), Some(2)], &[&[Some(2), Some(2), Some(2)]]);
        assert_eq!(answer.pairs.len(), 3);
        assert_eq!(answer.padded, [vec![Value::Integer(1), Value::Null]]);
    }

    /// Two instances setting bits in the one bitmap. A gathered row is padded only when neither
    /// of them matched it, which is what makes the driving side safe to run in parallel.
    #[test]
    fn a_padding_join_puts_the_bits_of_two_instances_together() {
        let answer =
            padding(JoinKind::Right, &[Some(1), Some(2), Some(3)], &[&[Some(1)], &[Some(3)]]);
        assert_eq!(answer.padded, [vec![Value::Integer(2), Value::Null]]);
    }

    /// Nothing drives it at all, so nothing matched anything and the whole gathered side is owed.
    #[test]
    fn a_padding_join_over_a_driving_side_with_no_rows_pads_every_gathered_row() {
        let answer = padding(JoinKind::Right, &[Some(1), Some(2)], &[]);
        assert_eq!(answer.pairs, Vec::<Vec<Value>>::new());
        assert_eq!(
            answer.padded,
            [vec![Value::Integer(1), Value::Null], vec![Value::Integer(2), Value::Null]]
        );
    }

    /// A gathered side with no rows in it owes nothing, and the table it built is the empty one
    /// every driving row misses in. A right join drops those rows and a full join keeps them.
    #[test]
    fn a_padding_join_over_an_empty_gathered_side_owes_nothing() {
        let right = padding(JoinKind::Right, &[], &[&[Some(1)]]);
        assert_eq!(right.pairs, Vec::<Vec<Value>>::new());
        assert_eq!(right.padded, Vec::<Vec<Value>>::new());
        let full = padding(JoinKind::Full, &[], &[&[Some(1)]]);
        assert_eq!(full.pairs, [vec![Value::Null, Value::Integer(1)]]);
        assert_eq!(full.padded, Vec::<Vec<Value>>::new());
    }

    /// The null rule, from both ends. Under `=` a null key matches nothing, so a gathered row
    /// whose key is null is never marked and is always padded, and a driving row whose key is
    /// null matches nothing either.
    #[test]
    fn a_padding_join_never_matches_a_key_that_is_null() {
        let answer = padding(JoinKind::Right, &[Some(1), None], &[&[Some(1), None]]);
        assert_eq!(answer.pairs, [vec![Value::Integer(1), Value::Integer(1)]]);
        assert_eq!(answer.padded, [vec![Value::Null, Value::Null]]);
    }

    /// The condition the lookup did not answer, applied to the candidates the equality found. A
    /// gathered row the residual threw away has not been matched, so it is owed a padded row, and
    /// getting that wrong is the difference between a right join and an inner one.
    #[test]
    fn a_padding_join_pads_a_gathered_row_the_residual_threw_away() {
        let mut plan = Plan::new();
        let (left, right) = (pair_schema(1), pair_schema(0));
        let conditions = {
            let one = column_at(&mut plan, 1, 0, LogicalType::Integer);
            let other = column_at(&mut plan, 0, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            let driving_g = column_at(&mut plan, 1, 1, LogicalType::Integer);
            let gathered_g = column_at(&mut plan, 0, 1, LogicalType::Integer);
            let over = greater(&mut plan, driving_g, gathered_g);
            plan.add_expr_list(&[key, over])
        };
        let memory = Memory::unlimited();
        let (keep, kept) = Keep::new(&memory);
        let mut local = keep.local();
        // Two gathered rows on the same key, told apart only by the column the residual reads.
        keep.sink(&pair_chunk(&[(7, 1), (7, 9), (8, 1)]), &mut local).expect("the gathered rows");
        keep.combine(local).expect("the one instance");
        keep.finalize(&rudb_pipeline::Lease::alone()).expect("the chunks");
        let pad = Padding::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
            JoinKind::Right,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("an equality beside a residual is still a lookup");

        // Key 7 finds both gathered rows and the residual keeps the one whose `g` is under 5. Key
        // 8 finds the third and the residual throws it away, so two of the three are owed.
        let answer = answered(&pad, &[pair_chunk(&[(7, 5), (8, 0)])], 4);
        assert_eq!(
            answer.pairs,
            [vec![Value::Integer(7), Value::Integer(1), Value::Integer(7), Value::Integer(5)]]
        );
        assert_eq!(
            answer.padded,
            [
                vec![Value::Integer(7), Value::Integer(9), Value::Null, Value::Null],
                vec![Value::Integer(8), Value::Integer(1), Value::Null, Value::Null]
            ]
        );
    }

    /// The drain hands over a chunk at a time rather than one row or the lot, so a gathered side
    /// nothing matched does not turn into an allocation the size of the side.
    #[test]
    fn a_padding_joins_drain_hands_over_a_vector_at_a_time() {
        let rows: Vec<Option<i32>> = (0..VECTOR_SIZE as i32 + 5).map(Some).collect();
        let answer = padding(JoinKind::Right, &rows, &[]);
        assert_eq!(answer.chunks, [VECTOR_SIZE, 5]);
        assert_eq!(answer.padded.len(), VECTOR_SIZE + 5);
    }

    /// No key for the scan under the driving side, and this is the one thing turning an outer
    /// join around costs. The rows such a filter removes are the ones that were going to match
    /// nothing, which is exactly the half of the answer the drain produces.
    #[test]
    fn a_padding_join_offers_no_key_to_the_scan() {
        let mut plan = Plan::new();
        let (left, right) = (schema("b", 1), schema("a", 0));
        let conditions = {
            let one = column(&mut plan, 1, LogicalType::Integer);
            let other = column(&mut plan, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        let kept = subject(&[Some(1)], &memory);
        let pad = Padding::new(
            &plan,
            &left,
            &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
            JoinKind::Right,
            conditions,
            &Cancel::new(),
            &memory,
        )
        .expect("one equality is enough to pair on");
        assert!(pad.sideways().is_empty(), "a padding join has no key it can offer");
    }

    /// The operator refuses the join it was not written for rather than answering it wrongly.
    /// Only `sides` puts a gathered side on the kept end and it only does it for these two, but
    /// the refusal is what makes that a fact about one pass rather than an agreement between two.
    #[test]
    fn a_padding_join_refuses_a_kind_it_does_not_answer() {
        let mut plan = Plan::new();
        let (left, right) = (schema("b", 1), schema("a", 0));
        let conditions = {
            let one = column(&mut plan, 1, LogicalType::Integer);
            let other = column(&mut plan, 0, LogicalType::Integer);
            let key = equal(&mut plan, one, other);
            plan.add_expr_list(&[key])
        };
        let memory = Memory::unlimited();
        for kind in
            [JoinKind::Inner, JoinKind::Left, JoinKind::Semi, JoinKind::Anti, JoinKind::Mark]
        {
            let kept = subject(&[Some(1)], &memory);
            let made = Padding::new(
                &plan,
                &left,
                &Gathered { schema: &right, chunks: kept, marker: None, swapped: true },
                kind,
                conditions,
                &Cancel::new(),
                &memory,
            );
            assert!(made.is_none(), "a {} join is not a padding join", kind.keyword());
        }
    }
}
