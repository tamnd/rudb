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
//! # A sorted bound rather than a heap
//!
//! The textbook answer is a binary heap of the bound, pushing every row and popping the worst. This
//! keeps the candidates sorted instead. A row first compares with the worst candidate and is
//! discarded without materializing its payload when it loses. A winner is inserted after existing
//! equal keys, which preserves input order for ties the same way the stable full sort does.
//!
//! Binary search makes the comparison cost `O(m log n)`, as it is for a heap. Inserting moves `n`
//! small row handles, but only a shrinking share of the input wins after the first `n` rows. The
//! important saving is that almost every row pays for its key and never becomes a heap-allocated
//! payload row. A large offset makes those moves expensive, so bounds above 64 keep the batched
//! sort and trim path until normalized keys make a heap cheap.
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

/// Above this bound, moving a sorted candidate array costs more than trimming in batches.
const SORTED_BOUND: usize = 64;

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
        // row at a time: the key still has the same Value layout the sort holds, and 2i (#63)
        // replaces it with one normalized comparable byte string per row.
        for row in 0..chunk.len() {
            let key: Vec<Value> = keys.iter().map(|column| column.value_at(row)).collect();
            if self.bound <= SORTED_BOUND {
                keep(&self.keys, &mut local.kept, key, chunk, row, self.bound, &mut local.failure);
            } else {
                let values: Vec<Value> = chunk.row(row).collect();
                taken += rows::footprint(&key) + rows::footprint(&values);
                local.kept.push((key, values));
            }
        }
        if self.bound <= SORTED_BOUND {
            recharge(&local.kept, &mut local.charged)?;
        } else {
            local.charged.grow(taken)?;
            if local.kept.len() > self.bound.saturating_mul(2) {
                trim(&self.keys, &mut local.kept, self.bound, &mut local.failure);
                recharge(&local.kept, &mut local.charged)?;
            }
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

/// Keeps one row when its key belongs in the ordered prefix.
fn keep(
    keys: &[SortKey],
    kept: &mut Vec<Sortable>,
    key: Vec<Value>,
    chunk: &Chunk,
    row: usize,
    bound: usize,
    failure: &mut Option<Error>,
) {
    if bound == 0 {
        return;
    }
    if kept.len() == bound
        && compare(keys, &key, &kept[bound - 1].0, failure) != std::cmp::Ordering::Less
    {
        return;
    }
    let at = kept.partition_point(|candidate| {
        compare(keys, &candidate.0, &key, failure) != std::cmp::Ordering::Greater
    });
    kept.insert(at, (key, chunk.row(row).collect()));
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
