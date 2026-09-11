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

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_kernels::{Connective, combine, is_true};
use rudb_plan::{ExprRef, JoinKind, Plan, Slice};
use rudb_vector::{Chunk, Vector};

use crate::expr::evaluate_all;
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// A join with a condition.
#[derive(Debug)]
pub(crate) struct Join<'a> {
    left: Box<dyn Operator + 'a>,
    right: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    kind: JoinKind,
    conditions: Vec<ExprRef>,
    left_schema: Schema,
    right_schema: Schema,
    combined: Schema,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
}

impl<'a> Join<'a> {
    pub(crate) fn new(
        plan: &'a Plan,
        left: Box<dyn Operator + 'a>,
        right: Box<dyn Operator + 'a>,
        kind: JoinKind,
        conditions: Slice,
    ) -> Self {
        let left_schema = left.schema().clone();
        let right_schema = right.schema().clone();
        let combined = Schema::concat(&left_schema, &right_schema);
        let schema = match kind {
            JoinKind::Semi | JoinKind::Anti => left_schema.clone(),
            _ => combined.clone(),
        };
        Self {
            left,
            right,
            plan,
            kind,
            conditions: plan.expr_list(conditions).to_vec(),
            left_schema,
            right_schema,
            combined,
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
        }
    }

    fn build(&mut self) -> Result<()> {
        let left_types = self.left_schema.types();
        let right_types = self.right_schema.types();
        let left_rows = rows::collect(self.left.as_mut())?;
        let right_rows = rows::collect(self.right.as_mut())?;
        if self.kind == JoinKind::Positional {
            self.chunks = rows::chunks(
                &self.schema.types(),
                &positional(&left_rows, &right_rows, left_types.len(), right_types.len()),
            )?;
            return Ok(());
        }
        let right_chunks = rows::chunks(&right_types, &right_rows)?;
        let mut matched = vec![false; right_rows.len()];
        let mut out: Vec<Vec<Value>> = Vec::new();
        for left_row in &left_rows {
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
        }
        if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            for (at, seen) in matched.iter().enumerate() {
                if !seen {
                    out.push(pad_left(left_types.len(), &right_rows[at]));
                }
            }
        }
        self.chunks = rows::chunks(&self.schema.types(), &out)?;
        Ok(())
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

impl Operator for Join<'_> {
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

/// An unconditional cross product.
///
/// The only join shaped operator here that does not materialize its left side. It holds the right
/// side, because that one has to be replayed once per left row, and then walks the left a row at a
/// time emitting one combined chunk per right chunk. A cross product of a thousand by a thousand is
/// a million rows and there is no way around producing them, but there is a way around holding them
/// all at once and this is it.
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
}

impl<'a> CrossProduct<'a> {
    pub(crate) fn new(left: Box<dyn Operator + 'a>, right: Box<dyn Operator + 'a>) -> Self {
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
