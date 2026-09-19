//! Sorting.
//!
//! A pipeline breaker: nothing can be emitted until the last row has arrived, because the last row
//! of the input can be the first row of the output. It reads everything, sorts, and then hands the
//! result out a chunk at a time.
//!
//! The sort is Rust's `sort_by`, which is stable. Stability is not something SQL promises and it is
//! kept anyway, because `ORDER BY a` over rows that tie on `a` producing a different order on two
//! runs of the same query is the kind of difference that makes a compatibility diff useless.
//!
//! # Keeping it stable on more than one thread
//!
//! A stable sort is stable in the order the rows were given to it, and on several threads that is
//! the order the threads happened to finish. So every row carries where it arrived from, which is
//! the morsel it came out of and its place in that morsel, and two rows that tie on every key are
//! separated by that instead. Reading it lexicographically is exactly the order one thread would
//! have produced, because one thread takes the morsels in the order they were cut and reads each
//! one through from the start, so a parallel sort answers what a serial sort answers rather than
//! answering something SQL also allows.
//!
//! A filter between the scan and the sort does not break that. The place a row carries is its place
//! among the rows that reached the sort rather than its row number in the file, and the rows that
//! reach the sort from one morsel still reach it in order.
//!
//! # The shape a sink has
//!
//! [`Sort`] is a [`Sink`], so the rows arrive through `sink`, one instance's rows are handed over
//! through `combine`, and `finalize` does the sort once after every instance has combined. On one
//! thread that is the same work in the same order as reading the input in a loop would be. On
//! several it is the shape that makes the sort possible at all, and having it now is why F4 changes
//! no operator.
//!
//! The finished chunks go into a [`Buffered`], which is a separate source rather than something
//! `finalize` hands back, for the reason [`Sink::finalize`] gives.

use std::cmp::Ordering;
use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Session, Value};
use rudb_pipeline::{Lease, Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;

/// One row on its way through a sort: the values of its keys, the row itself, and where it arrived.
///
/// row at a time: 2i (#63) sorts a normalized key that is one comparable byte string a row rather
/// than a `Vec<Value>`, and moves the payload by index at the end instead of carrying a copy of
/// every row through the sort.
pub(crate) type Sortable = (Vec<Value>, Vec<Value>, Arrival);

/// Where a row arrived: the morsel it came from and its place among the rows of that morsel.
///
/// Sixteen bytes beside two `Vec` headers and whatever they point at, which is why it is carried
/// per row rather than reconstructed. What it buys is that the answer does not depend on how many
/// threads ran.
pub(crate) type Arrival = (u64, u64);

/// An ordering over the input.
#[derive(Debug)]
pub(crate) struct Sort {
    keys: Vec<SortKey>,
    /// The key expressions, evaluated against the input's schema.
    exprs: Prepared,
    /// The input's types, which are also the output's, since a sort changes no column.
    types: Vec<LogicalType>,
    memory: Memory,
    /// Every instance's rows, waiting for the sort.
    rows: Mutex<Vec<Sortable>>,
    /// What those rows are charged, taken from the instances that gathered them and given back
    /// once the sorted chunks have been charged instead.
    charged: Mutex<Vec<Reservation>>,
    /// What the sorted chunks are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    out: Buffered,
}

/// What one instance of a sort gathers before it combines.
#[derive(Debug)]
pub(crate) struct Gathered {
    rows: Vec<Sortable>,
    scratch: Scratch,
    charged: Reservation,
    /// The morsel this instance is reading and how many of its rows have arrived.
    place: Place,
}

/// How far through a morsel an instance is, which is the second half of an [`Arrival`].
#[derive(Debug, Default)]
pub(crate) struct Place {
    pub(crate) morsel: u64,
    pub(crate) at: u64,
}

impl Place {
    /// Where the row at `row` of the chunk that starts here arrived.
    pub(crate) fn of(&self, row: usize) -> Arrival {
        (self.morsel, self.at.saturating_add(row as u64))
    }

    /// Moves past a chunk of `rows` rows of the same morsel.
    pub(crate) fn past(&mut self, rows: usize) {
        self.at = self.at.saturating_add(rows as u64);
    }

    /// Starts a new morsel.
    pub(crate) fn start(&mut self, morsel: u64) {
        self.morsel = morsel;
        self.at = 0;
    }
}

impl Sort {
    /// Applies the session semantics to the prepared sort keys.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.exprs = self.exprs.in_session(session);
        self
    }

    /// # Errors
    ///
    /// If a sort key does not resolve against the input's schema, which is a failure of the plan
    /// and is found here rather than on the first chunk.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        keys: Slice,
        memory: &Memory,
    ) -> Result<(Self, Buffered)> {
        let keys = plan.sort_key_list(keys).to_vec();
        let exprs: Vec<_> = keys.iter().map(|key| key.expr).collect();
        let out = Buffered::new();
        let sort = Self {
            exprs: Prepared::new(plan, &exprs, input)?,
            keys,
            types: input.types(),
            memory: memory.clone(),
            rows: Mutex::new(Vec::new()),
            charged: Mutex::new(Vec::new()),
            held: Mutex::new(memory.reservation()),
            out: out.clone(),
        };
        Ok((sort, out))
    }
}

impl Sink for Sort {
    type Local = Gathered;

    fn local(&self) -> Gathered {
        Gathered {
            rows: Vec::new(),
            scratch: self.exprs.scratch(),
            charged: self.memory.reservation(),
            place: Place::default(),
        }
    }

    fn at(&self, morsel: &rudb_pipeline::Morsel, local: &mut Gathered) -> Result<()> {
        local.place.start(morsel.index());
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathered) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        let mut taken = 0;
        // row at a time: see `Sortable`.
        for row in 0..chunk.len() {
            let key: Vec<Value> =
                keys.iter().map(|column| column.try_value_at(row)).collect::<Result<_>>()?;
            let values: Vec<Value> = (0..chunk.width())
                .map(|column| chunk.try_value_at(row, column))
                .collect::<Result<_>>()?;
            taken += rows::footprint(&key) + rows::footprint(&values);
            local.rows.push((key, values, local.place.of(row)));
        }
        local.place.past(chunk.len());
        local.charged.grow(taken)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        let mut rows = self.rows.lock().map_err(poisoned)?;
        // Appended rather than merged, because the sort has not happened yet. The order the
        // instances combine in does not decide anything, since every row carries where it arrived
        // and the comparison falls back to that when the keys tie.
        rows.extend(local.rows);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let mut sortable = std::mem::take(&mut *self.rows.lock().map_err(poisoned)?);
        let mut failure: Option<Error> = None;
        sortable.sort_by(|left, right| settled(&self.keys, left, right, &mut failure));
        if let Some(error) = failure {
            return Err(error);
        }
        let ordered: Vec<Vec<Value>> = sortable.into_iter().map(|(_, row, _)| row).collect();
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &ordered, &mut held)?;
        self.out.fill(chunks)?;
        // The gathered rows are gone and the chunks are charged instead, so what the instances
        // took is given back here and not before.
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
    }
}

/// Where two rows sit relative to each other, with a tie on every key settled by where they arrived.
///
/// This is what makes the answer the same however many threads ran. [`compare`] on its own leaves
/// tied rows to the stability of the sort, which is the order they were handed over, and that is the
/// order the threads finished in.
pub(crate) fn settled(
    keys: &[SortKey],
    left: &Sortable,
    right: &Sortable,
    failure: &mut Option<Error>,
) -> Ordering {
    match compare(keys, &left.0, &right.0, failure) {
        Ordering::Equal => left.2.cmp(&right.2),
        ordering => ordering,
    }
}

/// Where two rows of keys sit relative to each other, under the whole key list in priority order.
///
/// The first key that separates them decides, and rows that agree on every key are equal, which is
/// where the stability of the sort does the rest.
///
/// A comparison that fails is reported as equal and remembered in `failure`, because `sort_by` wants
/// a total order and has nowhere to put an error. The order that comes out of a run that failed is
/// not an order anybody looks at, since the caller returns the error instead of the rows.
pub(crate) fn compare(
    keys: &[SortKey],
    left: &[Value],
    right: &[Value],
    failure: &mut Option<Error>,
) -> Ordering {
    for (at, key) in keys.iter().enumerate() {
        let ordering = match rank(&left[at], &right[at], *key) {
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

/// Where two values sit relative to each other under one sort key.
///
/// The direction and the null placement are independent, which is the detail worth being careful
/// about. `ORDER BY x DESC NULLS LAST` is not `ORDER BY x NULLS FIRST` reversed: reversing the
/// whole comparison would move the nulls too, and DuckDB's answer keeps them where the query put
/// them. So the direction is applied to the comparison of two values and never to the rule that
/// places a null.
pub(crate) fn rank(left: &Value, right: &Value, key: SortKey) -> Result<Ordering> {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if key.nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if key.nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {
            let ordering = rudb_kernels::order(left, right)?;
            Ok(if key.descending { ordering.reverse() } else { ordering })
        }
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a thread panicked while holding the rows a sort is gathering")
}
