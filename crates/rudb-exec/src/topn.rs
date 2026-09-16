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
//! # Rejecting a chunk in one pass instead of a row at a time
//!
//! Almost every row of a million loses to the ten already held, and the cheap way to find that out
//! is the comparison kernel rather than a loop. Once the candidates are full, the worst of them is
//! a constant for the length of a chunk, so one vectorized comparison of the first key column
//! against that constant says which rows are still worth looking at, and on `ORDER BY EventTime
//! LIMIT 10` over ClickBench that is almost none of them after the first chunk. The rows it hands
//! back go down the same row at a time path as before, which is what keeps the answer the same.
//!
//! The bound moves while the chunk is being walked, since a winner replaces the worst candidate, so
//! what the pass produces is a superset of the rows that really win. That is the point: it is a
//! filter and not the decision, and every row it keeps is compared again properly.
//!
//! Three things make it step aside and look at every row instead. A worst candidate whose first key
//! is null, because then what beats it depends on where the query puts nulls and the comparison
//! kernel answers null rather than true. A `NULLS FIRST` ordering over a column that has nulls, for
//! the same reason read the other way: those rows win and a comparison against a value says nothing
//! about them. And a comparison the kernel refuses, which is left to the row path so that the error
//! comes out of the same place it came out of before. With more than one sort key the pass keeps
//! ties on the first one, since the second key can still turn a tie into a win.
//!
//! # The shape a sink has
//!
//! Like the sort, this is a [`Sink`]. The difference between them is where the trimming happens: an
//! instance trims what it holds as it goes, and `combine` trims again over what two instances
//! brought, which is what keeps the bound the bound rather than the bound times the number of
//! threads. On one thread there is one instance and the second trim does nothing.
//!
//! Every candidate carries where it arrived, for the reason the sort's own documentation gives: a
//! tie on every key is settled by that rather than by which thread got there first, so the ten rows
//! this hands back are the ten a single thread would have handed back. It costs sixteen bytes per
//! candidate and the bound is ten.

use std::cmp::Ordering;
use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_kernels::{Comparison, refine};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::{Chunk, Selection, Vector};

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;
use crate::sort::{Place, compare, rank, settled};

/// One row in the running: the values of its keys, the row itself, and where it arrived.
type Sortable = crate::sort::Sortable;

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
    /// The morsel this instance is reading and how many of its rows have arrived.
    place: Place,
}

impl TopN {
    /// Applies the session semantics to the prepared sort keys.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.exprs = self.exprs.in_session(session);
        self
    }

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
            place: Place::default(),
        }
    }

    fn at(&self, morsel: &rudb_pipeline::Morsel, local: &mut Running) -> Result<()> {
        local.place.start(morsel.index());
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Running) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        if self.bound <= SORTED_BOUND {
            let full = self.bound > 0 && local.kept.len() == self.bound;
            let narrowed = full
                .then(|| {
                    worth_looking_at(&self.keys, &keys, &local.kept[self.bound - 1].0, chunk.len())
                })
                .flatten();
            let failure = &mut local.failure;
            match narrowed {
                // row at a time: the rows the pass kept are the ones that can still win, and each
                // of them has to be placed among the candidates rather than counted.
                Some(rows) => {
                    for row in rows.iter() {
                        let arrival = local.place.of(row);
                        keep(
                            Where { keys: &self.keys, columns: &keys, chunk, row, arrival },
                            &mut local.kept,
                            self.bound,
                            failure,
                        );
                    }
                }
                // row at a time: the key still has the same Value layout the sort holds, and 2i
                // (#63) replaces it with one normalized comparable byte string per row.
                None => {
                    for row in 0..chunk.len() {
                        let arrival = local.place.of(row);
                        keep(
                            Where { keys: &self.keys, columns: &keys, chunk, row, arrival },
                            &mut local.kept,
                            self.bound,
                            failure,
                        );
                    }
                }
            }
            local.place.past(chunk.len());
            recharge(&local.kept, &mut local.charged)?;
            return Ok(Progress::More);
        }
        let mut taken = 0;
        // row at a time: the key still has the same Value layout the sort holds, and 2i (#63)
        // replaces it with one normalized comparable byte string per row.
        for row in 0..chunk.len() {
            let key: Vec<Value> =
                keys.iter().map(|column| column.try_value_at(row)).collect::<Result<_>>()?;
            let values: Vec<Value> = (0..chunk.width())
                .map(|column| chunk.try_value_at(row, column))
                .collect::<Result<_>>()?;
            taken += rows::footprint(&key) + rows::footprint(&values);
            local.kept.push((key, values, local.place.of(row)));
        }
        local.place.past(chunk.len());
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
        let ordered: Vec<Vec<Value>> = wanted.map(|(_, row, _)| row).collect();
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
///
/// Ties are settled by where the rows arrived rather than by the order they were handed over, which
/// is what makes the trim in `combine` give the same answer whichever thread combined first.
fn trim(keys: &[SortKey], kept: &mut Vec<Sortable>, bound: usize, failure: &mut Option<Error>) {
    kept.sort_by(|left, right| settled(keys, left, right, failure));
    kept.truncate(bound);
}

/// Keeps one row when its key belongs in the ordered prefix.
///
/// The key is read out of the columns a value at a time and only as far as the first key that
/// separates it from the worst candidate, so a row that loses on the first of three keys costs one
/// value rather than three and never allocates the `Vec` that holds them. Almost every row loses.
fn keep(
    Where { keys, columns, chunk, row, arrival }: Where<'_>,
    kept: &mut Vec<Sortable>,
    bound: usize,
    failure: &mut Option<Error>,
) {
    if bound == 0 {
        return;
    }
    if kept.len() == bound
        && against(keys, columns, row, &kept[bound - 1].0, failure) != Ordering::Less
    {
        return;
    }
    let key: Vec<Value> = match columns.iter().map(|column| column.try_value_at(row)).collect() {
        Ok(key) => key,
        Err(error) => {
            failure.get_or_insert(error);
            return;
        }
    };
    let values: Vec<Value> =
        match (0..chunk.width()).map(|column| chunk.try_value_at(row, column)).collect() {
            Ok(values) => values,
            Err(error) => {
                failure.get_or_insert(error);
                return;
            }
        };
    // After every candidate whose key it ties, which is where its arrival puts it too: an instance
    // reads the morsels it is given in order and each of them from the start, so a row reaching
    // here arrived after everything already held.
    let at = kept.partition_point(|candidate| {
        compare(keys, &candidate.0, &key, failure) != Ordering::Greater
    });
    kept.insert(at, (key, values, arrival));
    kept.truncate(bound);
}

/// One row being offered to the candidates, which is five things that only travel together.
struct Where<'a> {
    keys: &'a [SortKey],
    columns: &'a [Vector],
    chunk: &'a Chunk,
    row: usize,
    arrival: crate::sort::Arrival,
}

/// Where one row of the key columns sits against a key already held.
///
/// The same answer [`compare`] gives for the same two keys, read straight out of the columns rather
/// than out of a `Vec` built for the purpose.
fn against(
    keys: &[SortKey],
    columns: &[Vector],
    row: usize,
    held: &[Value],
    failure: &mut Option<Error>,
) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let value = match columns[at].try_value_at(row) {
            Ok(value) => value,
            Err(error) => {
                failure.get_or_insert(error);
                return Ordering::Equal;
            }
        };
        let ordering = match rank(&value, &held[at], *key) {
            Ok(ordering) => ordering,
            Err(error) => {
                failure.get_or_insert(error);
                Ordering::Equal
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// The rows of a chunk that can still beat `worst`, or nothing when every row has to be looked at.
///
/// One comparison of the first key column against a constant, for the reasons in the module doc.
fn worth_looking_at(
    keys: &[SortKey],
    columns: &[Vector],
    worst: &[Value],
    rows: usize,
) -> Option<Selection> {
    let key = *keys.first()?;
    let bound = worst.first()?;
    let column = columns.first()?;
    if bound.is_null() || (key.nulls_first && column.validity().has_nulls(rows)) {
        return None;
    }
    let op = match (key.descending, keys.len() == 1) {
        (false, true) => Comparison::Less,
        (false, false) => Comparison::LessOrEqual,
        (true, true) => Comparison::Greater,
        (true, false) => Comparison::GreaterOrEqual,
    };
    let against = Vector::constant(column.logical_type().clone(), bound.clone(), rows);
    refine(op, column, &against, &Selection::identity(rows)).ok()
}

/// Charges the scratch reservation for what is still held after a trim.
///
/// Released and taken again rather than shrunk, because a reservation gives everything back at once
/// and has no partial release. Nothing else can be holding the difference at this point, since the
/// operator is between two reads of its input.
fn recharge(kept: &[Sortable], scratch: &mut Reservation) -> Result<()> {
    let footprint =
        kept.iter().map(|(key, values, _)| rows::footprint(key) + rows::footprint(values)).sum();
    scratch.release();
    scratch.grow(footprint)
}
