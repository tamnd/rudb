//! `UNION`, `EXCEPT` and `INTERSECT`.
//!
//! The distinction that costs the most code here is `ALL`. Without it the operation is over sets
//! and the answer is a dedup. With it the operation is over multisets, and `EXCEPT ALL` of three
//! copies of a row minus one copy is two copies, not zero and not three. That rule is in the
//! standard, DuckDB implements it, and it is the sort of thing an implementation gets wrong once
//! and then nobody notices for a year because nothing in a normal query has duplicates in it.
//!
//! The output columns are the left side's, under the set operation's own table index. Both sides
//! were made type compatible by the binder, so nothing here casts anything.
//!
//! # Two pipelines and an edge
//!
//! This is the first operator with two inputs to move behind the push traits, and two inputs is two
//! pipelines. The right side ends in a [`Gather`](crate::gather::Gather), which holds its rows and
//! nothing else, and the left side ends here. The order is not a choice: every arm below needs the
//! whole right side before it can say anything about one left row, which is the dependency edge the
//! scheduler will read off the plan. Until there is a scheduler, `adapt::Paired` runs the two in
//! that order.

use std::sync::Mutex;

use rudb_common::{Error, Memory, Reservation, Result, Value};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::SetOpKind;
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::gather::{self, Gathering, Rows};
use crate::key::{Key, RowMap, RowSet};
use crate::rows;
use crate::schema::Schema;

/// A set operation over two inputs.
#[derive(Debug)]
pub(crate) struct SetOp {
    kind: SetOpKind,
    all: bool,
    schema: Schema,
    memory: Memory,
    /// The right side, filled by the pipeline this one depends on.
    right: Rows,
    /// The left side, as every instance gathered it.
    left: Mutex<Vec<Vec<Value>>>,
    /// What the left side is charged, given back once the finished chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the finished chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

impl SetOp {
    /// The sink for the left side, and the source the answer comes out of.
    ///
    /// `left` is the left input's schema, whose fields become the output's under `index`, and
    /// `right` is the handle on the rows the other pipeline gathered.
    pub(crate) fn new(
        left: &Schema,
        right: Rows,
        kind: SetOpKind,
        all: bool,
        index: u32,
        memory: &Memory,
    ) -> (Self, Buffered) {
        let out = Buffered::new();
        let setop = Self {
            kind,
            all,
            schema: Schema::numbered(left.fields().to_vec(), index),
            memory: memory.clone(),
            right,
            left: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        (setop, out)
    }

    /// What this operator produces, which is the left side's columns under its own table index.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }
}

impl Sink for SetOp {
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
        // Both sides at once, which is what every arm below needs, and the counting tables on top
        // of them. The tables are not charged separately, because a count per distinct row is
        // bounded by the rows that are already charged and charging it twice would refuse a query
        // that fits.
        let left = std::mem::take(&mut *self.left.lock().map_err(poisoned)?);
        let right = self.right.take()?;
        let out = match (self.kind, self.all) {
            (SetOpKind::Union, true) => {
                let mut out = left;
                out.extend(right);
                out
            }
            (SetOpKind::Union, false) => {
                let mut out = left;
                out.extend(right);
                deduplicated(out)
            }
            (SetOpKind::Except, true) => difference(left, &counts(&right)),
            (SetOpKind::Except, false) => {
                let held = counts(&right);
                deduplicated(
                    left.into_iter().filter(|row| !held.contains_key(&Key(row.clone()))).collect(),
                )
            }
            (SetOpKind::Intersect, true) => intersection(left, &counts(&right)),
            (SetOpKind::Intersect, false) => {
                let held = counts(&right);
                deduplicated(
                    left.into_iter().filter(|row| held.contains_key(&Key(row.clone()))).collect(),
                )
            }
        };
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.schema.types(), &out, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a set operation gathered")
}

/// How many times each row appears.
fn counts(rows: &[Vec<Value>]) -> RowMap<usize> {
    let mut held = RowMap::default();
    for row in rows {
        *held.entry(Key(row.clone())).or_insert(0) += 1;
    }
    held
}

/// The first occurrence of each row, in the order they arrived.
fn deduplicated(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut seen = RowSet::default();
    rows.into_iter().filter(|row| seen.insert(Key(row.clone()))).collect()
}

/// `EXCEPT ALL`: each left row survives unless a right row has already cancelled it.
fn difference(left: Vec<Vec<Value>>, right: &RowMap<usize>) -> Vec<Vec<Value>> {
    let mut budget = right.clone();
    let mut out = Vec::new();
    for row in left {
        match budget.get_mut(&Key(row.clone())) {
            Some(remaining) if *remaining > 0 => *remaining -= 1,
            _ => out.push(row),
        }
    }
    out
}

/// `INTERSECT ALL`: a left row survives while the right side still has a copy to pair it with.
fn intersection(left: Vec<Vec<Value>>, right: &RowMap<usize>) -> Vec<Vec<Value>> {
    let mut budget = right.clone();
    let mut out = Vec::new();
    for row in left {
        if let Some(remaining) = budget.get_mut(&Key(row.clone())) {
            if *remaining > 0 {
                *remaining -= 1;
                out.push(row);
            }
        }
    }
    out
}
