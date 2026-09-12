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

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Value};
use rudb_pipeline::{Progress, Sink};
use rudb_plan::{Plan, Slice, SortKey};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::prepared::{Prepared, Scratch};
use crate::rows;
use crate::schema::Schema;

/// One row on its way through a sort: the values of its keys, and the row itself.
///
/// row at a time: 2i (#63) sorts a normalized key that is one comparable byte string a row rather
/// than a `Vec<Value>`, and moves the payload by index at the end instead of carrying a copy of
/// every row through the sort.
type Sortable = (Vec<Value>, Vec<Value>);

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
}

impl Sort {
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
        }
    }

    fn sink(&self, chunk: &Chunk, local: &mut Gathered) -> Result<Progress> {
        let mut keys = Vec::with_capacity(self.keys.len());
        self.exprs.evaluate(chunk, &mut local.scratch, &mut keys)?;
        let mut taken = 0;
        // row at a time: see `Sortable`.
        for row in 0..chunk.len() {
            let key: Vec<Value> = keys.iter().map(|column| column.value_at(row)).collect();
            let values: Vec<Value> = chunk.row(row).collect();
            taken += rows::footprint(&key) + rows::footprint(&values);
            local.rows.push((key, values));
        }
        local.charged.grow(taken)?;
        Ok(Progress::More)
    }

    fn combine(&self, local: Gathered) -> Result<()> {
        let mut rows = self.rows.lock().map_err(poisoned)?;
        // Appended rather than merged, because the sort has not happened yet. What order the
        // instances combine in is what decides how rows that tie on every key come out, which is
        // why F4 will have to combine in a fixed order and not in the order threads finish.
        rows.extend(local.rows);
        self.charged.lock().map_err(poisoned)?.push(local.charged);
        Ok(())
    }

    fn finalize(&self) -> Result<()> {
        let mut sortable = std::mem::take(&mut *self.rows.lock().map_err(poisoned)?);
        let mut failure: Option<Error> = None;
        sortable.sort_by(|left, right| compare(&self.keys, &left.0, &right.0, &mut failure));
        if let Some(error) = failure {
            return Err(error);
        }
        let ordered: Vec<Vec<Value>> = sortable.into_iter().map(|(_, row)| row).collect();
        let mut held = self.held.lock().map_err(poisoned)?;
        let chunks = rows::chunks(&self.types, &ordered, &mut held)?;
        self.out.fill(chunks)?;
        // The gathered rows are gone and the chunks are charged instead, so what the instances
        // took is given back here and not before.
        self.charged.lock().map_err(poisoned)?.clear();
        Ok(())
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
fn rank(left: &Value, right: &Value, key: SortKey) -> Result<Ordering> {
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
