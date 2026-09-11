//! Sorting when only the first few rows are wanted.
//!
//! `ORDER BY x LIMIT 10` over a hundred million rows has the same answer as a sort with a limit over
//! it and nothing like the same cost. A sort holds every row it was given, because the last row of
//! the input can be the first row of the output, so the memory it takes is the size of the input and
//! the time it takes is a full sort of it. Ten rows are wanted. This holds the rows that could still
//! come out and throws the rest away as it goes.
//!
//! What has to be held is `count + offset` rows, not `count`, since the rows that are skipped still
//! have to be found before there is anything to skip them from.
//!
//! # Sort and trim rather than a heap
//!
//! The textbook answer is a binary heap of the bound, pushing every row and popping the worst. This
//! collects rows until it holds twice the bound, then sorts and keeps the better half. The reason is
//! stability. `crate::sort` promises that rows tying on every key come out in input order, because a
//! query that returns a different order on two runs makes a compatibility diff useless, and a heap
//! does not promise that: sifting moves equal elements past each other. Sorting and truncating keeps
//! it, since the sort is stable and truncation keeps a prefix, and rows that arrive later are
//! appended after the survivors so their relative order is the order they came in.
//!
//! It also costs about the same. Each trim sorts `2n` rows and drops `n`, so a run of `m` rows is
//! `m / n` sorts of `2n`, which is `O(m log n)` with the same constant a heap would pay on a row
//! comparison that walks a `Vec<Value>` per key.
//!
//! What is held is twice the bound rather than the bound, so that a trim is amortized over `n` rows
//! rather than run on every row after the first `n`.

use rudb_common::{Error, Memory, Reservation, Result, Value};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::Chunk;

use crate::expr::evaluate_all;
use crate::operator::Operator;
use crate::rows;
use crate::schema::Schema;
use crate::sort::compare;

/// The first rows of an ordering, without holding the rest.
#[derive(Debug)]
pub(crate) struct TopN<'a> {
    input: Box<dyn Operator + 'a>,
    plan: &'a Plan,
    keys: Vec<SortKey>,
    schema: Schema,
    /// How many rows to emit, once the ones to skip have been skipped.
    count: usize,
    /// How many rows to skip first.
    offset: usize,
    /// `count + offset`, which is how many rows can still turn out to be wanted.
    bound: usize,
    built: bool,
    chunks: Vec<Chunk>,
    at: usize,
    memory: Memory,
    /// What the finished chunks are charged, held for as long as this operator holds them.
    held: Reservation,
}

impl<'a> TopN<'a> {
    pub(crate) fn new(
        plan: &'a Plan,
        input: Box<dyn Operator + 'a>,
        keys: Slice,
        count: u64,
        offset: u64,
        memory: &Memory,
    ) -> Self {
        let schema = input.schema().clone();
        // A limit past what a `Vec` can hold is a limit nothing reaches, so saturating here turns a
        // bound nobody can hit into the largest one this machine has room for, and the operator
        // degenerates into the sort it would have been.
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        Self {
            input,
            plan,
            keys: plan.sort_key_list(keys).to_vec(),
            schema,
            count,
            offset,
            bound: count.saturating_add(offset),
            built: false,
            chunks: Vec::new(),
            at: 0,
            memory: memory.clone(),
            held: memory.reservation(),
        }
    }

    fn build(&mut self) -> Result<()> {
        let exprs: Vec<_> = self.keys.iter().map(|key| key.expr).collect();
        // The rows still in the running, charged separately from the finished chunks below, because
        // this one is given back the moment the last trim is done with it.
        let mut scratch = self.memory.reservation();
        let mut kept: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
        let mut failure: Option<Error> = None;
        let ceiling = self.bound.saturating_mul(2);
        while let Some(chunk) = self.input.next()? {
            let keys = evaluate_all(self.plan, &exprs, &self.schema, &chunk)?;
            let mut taken = 0;
            // row at a time: the same layout the sort holds, and 2i (#63) replaces both at once with
            // a normalized key that is one comparable byte string a row and a payload beside it.
            for row in 0..chunk.len() {
                let key: Vec<Value> = keys.iter().map(|column| column.value_at(row)).collect();
                let values: Vec<Value> = chunk.row(row).collect();
                taken += rows::footprint(&key) + rows::footprint(&values);
                kept.push((key, values));
            }
            scratch.grow(taken)?;
            if kept.len() > ceiling {
                trim(&self.keys, &mut kept, self.bound, &mut failure);
                recharge(&kept, &mut scratch)?;
            }
        }
        trim(&self.keys, &mut kept, self.bound, &mut failure);
        if let Some(error) = failure {
            return Err(error);
        }
        let wanted = kept.into_iter().skip(self.offset).take(self.count);
        let ordered: Vec<Vec<Value>> = wanted.map(|(_, row)| row).collect();
        self.chunks = rows::chunks(&self.schema.types(), &ordered, &mut self.held)?;
        Ok(())
    }
}

/// Orders what is held and keeps the first `bound` of it.
fn trim(
    keys: &[SortKey],
    kept: &mut Vec<(Vec<Value>, Vec<Value>)>,
    bound: usize,
    failure: &mut Option<Error>,
) {
    kept.sort_by(|left, right| compare(keys, &left.0, &right.0, failure));
    kept.truncate(bound);
}

/// Charges the scratch reservation for what is still held after a trim.
///
/// Released and taken again rather than shrunk, because a reservation gives everything back at once
/// and has no partial release. Nothing else can be holding the difference at this point, since the
/// operator is between two reads of its input.
fn recharge(kept: &[(Vec<Value>, Vec<Value>)], scratch: &mut Reservation) -> Result<()> {
    let footprint =
        kept.iter().map(|(key, values)| rows::footprint(key) + rows::footprint(values)).sum();
    scratch.release();
    scratch.grow(footprint)
}

impl Operator for TopN<'_> {
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
