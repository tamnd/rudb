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

use rudb_common::{Memory, Reservation, Result, Value};
use rudb_plan::SetOpKind;
use rudb_vector::Chunk;

use crate::key::{Key, RowMap, RowSet};
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;

/// A set operation over two inputs.
#[derive(Debug)]
pub(crate) struct SetOp<'a> {
    left: Box<dyn Operator + 'a>,
    right: Box<dyn Operator + 'a>,
    kind: SetOpKind,
    all: bool,
    schema: Schema,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
    memory: Memory,
    /// What the finished chunks are charged, held for as long as this operator holds them.
    held: Reservation,
}

impl<'a> SetOp<'a> {
    pub(crate) fn new(
        left: Box<dyn Operator + 'a>,
        right: Box<dyn Operator + 'a>,
        kind: SetOpKind,
        all: bool,
        index: u32,
        memory: &Memory,
    ) -> Self {
        let schema = Schema::numbered(left.schema().fields().to_vec(), index);
        Self {
            left,
            right,
            kind,
            all,
            schema,
            built: false,
            chunks: Vec::new(),
            at: 0,
            memory: memory.clone(),
            held: memory.reservation(),
        }
    }

    fn build(&mut self) -> Result<()> {
        // Both sides at once, which is what every arm below needs, and the counting tables on top
        // of them. The tables are not charged separately, because a count per distinct row is
        // bounded by the rows that are already charged and charging it twice would refuse a query
        // that fits.
        let mut scratch = self.memory.reservation();
        let left = rows::collect(self.left.as_mut(), &mut scratch)?;
        let right = rows::collect(self.right.as_mut(), &mut scratch)?;
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
        self.chunks = rows::chunks(&self.schema.types(), &out, &mut self.held)?;
        Ok(())
    }
}

impl Operator for SetOp<'_> {
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
