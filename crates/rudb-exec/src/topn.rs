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
//!
//! # The shape a sink has
//!
//! Like the sort, this is a [`Sink`]. The difference between them is where the trimming happens: an
//! instance trims what it holds as it goes, and `combine` trims again over what two instances
//! brought, which is what keeps the bound the bound rather than the bound times the number of
//! threads. On one thread there is one instance and the second trim does nothing.

use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Value};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::sort::compare;

/// One row in the running: the values of its keys, and the row itself.
type Sortable = (Vec<Value>, Vec<Value>);

/// The first rows of an ordering, without holding the rest.
#[derive(Debug)]
pub(crate) struct TopN {
    keys: Vec<SortKey>,
    /// The key expressions, evaluated against the input's schema.
    exprs: Prepared,
    /// The input's types, which are also the output's.
    types: Vec<LogicalType>,
    /// How many rows to emit, once the ones to skip have been skipped.
    count: usize,
    /// How many rows to skip first.
    offset: usize,
    /// `count + offset`, which is how many rows can still turn out to be wanted.
    bound: usize,
    memory: Memory,
    /// What every instance brought, already trimmed to the bound.
    rows: Mutex<Vec<Sortable>>,
    /// What those rows are charged, given back once the finished chunks are charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the finished chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// What one instance of a top N holds while it runs.
#[derive(Debug)]
pub(crate) struct Running {
    kept: Vec<Sortable>,
    scratch: Scratch,
    charged: Reservation,
    failure: Option<Error>,
}

impl TopN {
    /// # Errors
    ///
    /// If a sort key does not resolve against the input's schema.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        keys: Slice,
        count: u64,
        offset: u64,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        // A limit past what a `Vec` can hold is a limit nothing reaches, so saturating here turns a
        // bound nobody can hit into the largest one this machine has room for, and the operator
        // degenerates into the sort it would have been.
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let offset = usize::try_from(offset).unwrap_or(usize::MAX);
        let keys = plan.sort_key_list(keys).to_vec();
        let exprs: Vec<_> = keys.iter().map(|key| key.expr).collect();
        let out = Buffered::new();
        let top = Self {
            exprs: Prepared::new(plan, &exprs, input)?,
            keys,
            types: input.types(),
            count,
            offset,
            bound: count.saturating_add(offset),
            memory: memory.clone(),
            rows: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Ok((top, out))
    }
}

impl Sink for TopN {
    type Local = Running;

    fn local(&self) -> Running {
        Running {
            kept: Vec::new(),
            scratch: self.exprs.scratch(),
            charged: self.memory.reservation(),
            failure: None,
        }
    }

    fn sink(&self, chunk: &Chunk, local: &mut Running) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        let mut taken = 0;
        // row at a time: the same layout the sort holds, and 2i (#63) replaces both at once with a
        // normalized key that is one comparable byte string a row and a payload beside it.
        for row in 0..chunk.len() {
            let key: Vec<Value> = keys.iter().map(|column| column.value_at(row)).collect();
            let values: Vec<Value> = chunk.row(row).collect();
            taken += rows::footprint(&key) + rows::footprint(&values);
            local.kept.push((key, values));
        }
        local.charged.grow(taken)?;
        if local.kept.len() > self.bound.saturating_mul(2) {
            trim(&self.keys, &mut local.kept, self.bound, &mut local.failure);
            recharge(&local.kept, &mut local.charged)?;
        }
        Ok(Progress::More)
    }

    fn combine(&self, mut local: Running) -> Result<()> {
        if let Some(error) = local.failure {
            return Err(error);
        }
        let mut rows = self.rows.lock().map_err(poisoned)?;
        rows.extend(local.kept);
        // Trimmed here as well as in the instance, so that combining thirty two instances holding
        // the bound each leaves the bound and not thirty two times it.
        let mut failure = None;
        trim(&self.keys, &mut rows, self.bound, &mut failure);
        if let Some(error) = failure {
            return Err(error);
        }
        recharge(&rows, &mut local.charged)?;
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        let kept = std::mem::take(&mut *self.rows.lock().map_err(poisoned)?);
        let wanted = kept.into_iter().skip(self.offset).take(self.count);
        let ordered: Vec<Vec<Value>> = wanted.map(|(_, row)| row).collect();
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &ordered, &mut held)?;
        self.out.fill(chunks)?;
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a top N is keeping")
}

/// Orders what is held and keeps the first `bound` of it.
fn trim(keys: &[SortKey], kept: &mut Vec<Sortable>, bound: usize, failure: &mut Option<Error>) {
    kept.sort_by(|left, right| compare(keys, &left.0, &right.0, failure));
    kept.truncate(bound);
}

/// Charges the scratch reservation for what is still held after a trim.
///
/// Released and taken again rather than shrunk, because a reservation gives everything back at once
/// and has no partial release. Nothing else can be holding the difference at this point, since the
/// operator is between two reads of its input.
fn recharge(kept: &[Sortable], scratch: &mut Reservation) -> Result<()> {
    let footprint =
        kept.iter().map(|(key, values)| rows::footprint(key) + rows::footprint(values)).sum();
    scratch.release();
    scratch.grow(footprint)
}
