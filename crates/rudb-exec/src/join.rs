//! Joins.
//!
//! Every join here is a nested loop, and that is the one place in this crate where the M0 operator
//! is not merely slower than what replaces it but asymptotically worse. `spec/07-execution.md`
//! section 7.4's hash join is M1 work and it is the single largest performance item in the
//! executor, because a join is what every query past the simplest one is mostly made of.
//!
//! It is written this way first for the same reason as everything else here: eight join kinds each
//! have their own rule about what happens to a row with no match, and getting those eight rules
//! right in a nested loop is a page of code that can be read against the standard. Getting them
//! right in a hash join with a build side, a probe side, a match bitmap and a spill boundary is not,
//! and the way to find out whether the hash join has them right is to run both and diff.
//!
//! The condition is evaluated over the left row paired with a whole chunk of the right side rather
//! than one right row at a time, which keeps the evaluator on its batch interface and makes the
//! left side's columns constant vectors that cost one value each.
//!
//! # Two pipelines and an edge
//!
//! A join is two inputs, and two inputs is two pipelines with a dependency between them. The right
//! side ends in a [`Gather`](crate::gather::Gather), which keeps its rows and does nothing else, and
//! the left side ends here. The order is not a choice: no left row can be answered until every right
//! row it might match has been seen, and that is the edge the scheduler will read off the plan. It
//! is also the edge the hash join in #62 builds on, with the build side where the gather is now.
//!
//! The left side is held whole as well, which the nested loop always did and the hash join will not.
//! What replaces it is a probe that runs a chunk at a time and needs no state past the match bitmap,
//! and the shape here is already the one that wants: the rows arrive at [`Sink::sink`] chunk by
//! chunk, and it is [`Sink::finalize`] that keeps them rather than the interface.

use std::sync::Mutex;

use rudb_common::{Cancel, Error, LogicalType, Memory, Reservation, Result, Value};
use rudb_kernels::{Connective, combine, is_true};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{ExprRef, JoinKind, Plan, Slice};
use rudb_vector::{Chunk, Vector};

use crate::buffer::Buffered;
use crate::expr::evaluate_all;
use crate::gather::{self, Gathering, Rows};
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// A join with a condition.
#[derive(Debug)]
pub(crate) struct Join<'a> {
    plan: &'a Plan,
    kind: JoinKind,
    conditions: Vec<ExprRef>,
    left_schema: Schema,
    right_schema: Schema,
    combined: Schema,
    schema: Schema,
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
}

impl<'a> Join<'a> {
    /// The sink for the left side, and the source the answer comes out of.
    ///
    /// `left` is the left input's schema and `right` is the side the pipeline before this one
    /// gathered.
    pub(crate) fn new(
        plan: &'a Plan,
        left: &Schema,
        right: Gathered<'_>,
        kind: JoinKind,
        conditions: Slice,
        cancel: &Cancel,
        memory: &Memory,
    ) -> (Self, Buffered) {
        let combined = Schema::concat(left, right.schema);
        let schema = match kind {
            JoinKind::Semi | JoinKind::Anti => left.clone(),
            _ => combined.clone(),
        };
        let out = Buffered::new();
        let join = Self {
            plan,
            kind,
            conditions: plan.expr_list(conditions).to_vec(),
            left_schema: left.clone(),
            right_schema: right.schema.clone(),
            combined,
            schema,
            memory: memory.clone(),
            cancel: cancel.clone(),
            right: right.rows,
            left: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        (join, out)
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
        let right_chunks = rows::chunks(&right_types, right_rows, &mut scratch)?;
        let mut matched = vec![false; right_rows.len()];
        scratch.grow(u64::try_from(right_rows.len()).unwrap_or(u64::MAX))?;
        let mut out: Vec<Vec<Value>> = Vec::new();
        for left_row in left_rows {
            // Once per left row, in the same place and for the same reason as the reservation at
            // the bottom of the loop. What a query can run past its clock by is one pass over the
            // right side, which is the smallest unit this loop has that is not the inner one.
            self.cancel.check()?;
            let before = out.len();
            let hits = self.matching(left_row, &left_types, &right_chunks)?;
            for &hit in &hits {
                matched[hit] = true;
            }
            match self.kind {
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
                        return Err(Error::invalid_input(
                            "More than one row returned by a subquery used as an expression"
                                .to_string(),
                        ));
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
                    for &hit in &hits {
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
                let flags = evaluate_all(self.plan, &self.conditions, &self.combined, &combined)?;
                let merged = combine(Connective::And, &flags)?;
                // row at a time: this is the nested loop join, which is the join that exists until
                // 2h (#62) builds the hash join on top of 2f's table. The flags are already a
                // vector here, so what this wants is the selection that 2c (#57) threads.
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
}

impl Sink for Join<'_> {
    type Local = Gathering;

    fn local(&self) -> Gathering {
        gather::gathering(&self.memory)
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
        let out = self.joined(&left_rows, &right_rows)?;
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
/// The only join shaped operator here that does not materialize its left side. It holds the right
/// side, because that one has to be replayed once per left row, and then walks the left a row at a
/// time emitting one combined chunk per right chunk. A cross product of a thousand by a thousand is
/// a million rows and there is no way around producing them, but there is a way around holding them
/// all at once and this is it.
///
/// It is also the one operator here that is still pulled, and the reason is that property. One input
/// chunk becomes many output chunks, and neither [`Sink`] nor [`rudb_pipeline::Stream`] can say that
/// yet: a stream transforms one chunk into one chunk, and a sink that held the answer would hold the
/// million rows this is written to avoid. What it needs is a way for an operator to tell the driver
/// that it has more output for the input it was already given, which is a change to the pipeline
/// crate rather than to this file, and it is the next one.
#[derive(Debug)]
pub(crate) struct CrossProduct<'a> {
    left: Box<dyn Operator + 'a>,
    right: Box<dyn Operator + 'a>,
    left_types: Vec<LogicalType>,
    schema: Schema,
    stored: Vec<Chunk>,
    prepared: bool,
    current: Option<Chunk>,
    left_row: usize,
    right_chunk: usize,
    /// What the stored right side is charged. The output is not charged, because this operator
    /// hands each chunk out and forgets it.
    held: Reservation,
}

impl<'a> CrossProduct<'a> {
    pub(crate) fn new(
        left: Box<dyn Operator + 'a>,
        right: Box<dyn Operator + 'a>,
        memory: &Memory,
    ) -> Self {
        let left_schema = left.schema().clone();
        let schema = Schema::concat(&left_schema, right.schema());
        Self {
            left,
            right,
            left_types: left_schema.types(),
            schema,
            stored: Vec::new(),
            prepared: false,
            current: None,
            left_row: 0,
            right_chunk: 0,
            held: memory.reservation(),
        }
    }
}

impl Operator for CrossProduct<'_> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.prepared {
            while let Some(chunk) = self.right.next()? {
                if !chunk.is_empty() {
                    self.held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
                    self.stored.push(chunk);
                }
            }
            self.prepared = true;
        }
        if self.stored.is_empty() {
            return Ok(None);
        }
        loop {
            let Some(chunk) = &self.current else {
                match self.left.next()? {
                    Some(chunk) => {
                        self.current = Some(chunk);
                        self.left_row = 0;
                        self.right_chunk = 0;
                    }
                    None => return Ok(None),
                }
                continue;
            };
            if self.left_row >= chunk.len() {
                self.current = None;
                continue;
            }
            let left_row: Vec<Value> = chunk.row(self.left_row).collect();
            let out = widen(&left_row, &self.left_types, &self.stored[self.right_chunk])?;
            self.right_chunk += 1;
            if self.right_chunk >= self.stored.len() {
                self.right_chunk = 0;
                self.left_row += 1;
            }
            return Ok(Some(out));
        }
    }
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
    use rudb_plan::{JoinKind, Plan, Slice};
    use rudb_vector::{Data, Vector};

    use super::{Buffered, Chunk, Gathered, Join, Schema, Sink};
    use crate::gather::{Gather, Rows};

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
            Gathered { schema: &schema("b", 1), rows: right },
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
            Gathered { schema: &schema("b", 1), rows: right },
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
            Gathered { schema: &schema("b", 1), rows: right },
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
}
