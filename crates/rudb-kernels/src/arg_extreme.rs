//! `arg_min` and `arg_max`, with their `_null` and `_nulls_last` spellings, and the forms with a
//! third argument that answer the `n` best rows as a list.
//!
//! The two argument form keeps one row: the `arg` of the row whose `by` is the least, or the
//! greatest, with the row that arrived first kept on a tie. The three spellings differ only in
//! which nulls they look at, and each rule below is the pin's, read off its answers:
//!
//! - `arg_min` skips a row where either side is null.
//! - `arg_min_null` skips a row whose `by` is null and keeps one whose `arg` is, so it can answer
//!   null.
//! - `arg_min_nulls_last` keeps a row with a null `by` only while it has nothing better, and the
//!   first such row is the one it keeps, unless its `arg` is null too.
//!
//! The form with `n` holds a heap of at most `n` rows ordered the way the pin's is. The pin keeps it
//! with `std::push_heap`, `std::pop_heap` and `std::sort_heap` from libstdc++, and which of two
//! tied rows survives and the order ties come out in is whatever those three algorithms do. So
//! [`push_heap`], [`adjust_heap`] and [`sort_heap`] below are those algorithms step for step, which
//! is what makes `arg_min(a, b, 5)` answer `[4, 2, 6, 7, 3]` over the pin's own test rows.

use rudb_common::{Error, LogicalType, Result, Value};

use crate::compare::{float_order, order};
use crate::number::integral;
use crate::quantile::{Column, Whole};

/// The largest `n` the pin takes.
const MOST: i64 = 1_000_000;

/// Which nulls a call looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nulls {
    /// `arg_min`, which skips any row with a null in it.
    Skip,
    /// `arg_min_null`, which keeps a null `arg`.
    Arg,
    /// `arg_min_nulls_last`, which keeps a null `by` too, below every other.
    Last,
}

/// The state of one call.
#[derive(Debug, Clone)]
pub(crate) enum ArgExtreme {
    /// The two argument form, or a call that has not seen a row yet.
    One {
        /// Whether a row has been kept.
        set: bool,
        /// The kept row's `arg`, which is null when the row's was.
        arg: Value,
        /// The kept row's `by`, or `None` when it was null.
        by: Option<Value>,
        least: bool,
        nulls: Nulls,
    },
    /// The form with `n`, as the pin's heap of `(by, arg)` pairs.
    Many { heap: Vec<(Value, Value)>, capacity: usize, least: bool, nulls: Nulls },
}

impl ArgExtreme {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        let (least, nulls) = match name {
            "arg_min" => (true, Nulls::Skip),
            "arg_max" => (false, Nulls::Skip),
            "arg_min_null" => (true, Nulls::Arg),
            "arg_max_null" => (false, Nulls::Arg),
            "arg_min_nulls_last" => (true, Nulls::Last),
            "arg_max_nulls_last" => (false, Nulls::Last),
            _ => return None,
        };
        Some(Self::One { set: false, arg: Value::Null, by: None, least, nulls })
    }

    /// Whether a row whose `by` is `key` is sure to leave this state as it is, which lets a column
    /// skip the row before a value is made for either argument.
    ///
    /// Only a state that already holds a key it can weigh `key` against says yes, and only when
    /// `key` is not strictly better, since a tie keeps what came first. A null held key, a heap that
    /// is not full yet and a call with `n` that has not read it all say no, and the row goes the
    /// slow way. `arity` is how many arguments the call has.
    pub(crate) fn cannot_take(&self, key: Key, arity: usize) -> bool {
        match self {
            Self::One { set: true, by: Some(held), least, .. } if arity == 2 => {
                key.beats(held, *least) == Some(false)
            }
            Self::Many { heap, capacity, least, .. } if heap.len() >= *capacity => {
                heap.first().is_some_and(|(held, _)| key.beats(held, *least) == Some(false))
            }
            _ => false,
        }
    }

    /// Folds one row in, `arg`, `by` and for the list form `n`.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let (Some(arg), Some(by)) = (args.first(), args.get(1)) else {
            return Err(Error::internal("arg_min over fewer than 2 arguments"));
        };
        if let (Self::One { least, nulls, .. }, Some(n)) = (&*self, args.get(2)) {
            let (least, nulls) = (*least, *nulls);
            if skipped(nulls, arg, by) {
                return Ok(());
            }
            let capacity = capacity(n)?;
            *self = Self::Many { heap: Vec::new(), capacity, least, nulls };
        }
        match self {
            Self::One { set, arg: held, by: kept, least, nulls } => {
                if skipped(*nulls, arg, by) {
                    return Ok(());
                }
                if by.is_null() {
                    // Only a call that keeps null `by` values gets here. Such a row is kept while
                    // there is nothing at all, and never displaces anything.
                    if !*set && !arg.is_null() {
                        (*set, *held, *kept) = (true, arg.clone(), None);
                    }
                    return Ok(());
                }
                let better = match kept {
                    Some(kept) if *set => beats(by, kept, *least)?,
                    _ => true,
                };
                if better {
                    (*set, *held, *kept) = (true, arg.clone(), Some(by.clone()));
                }
            }
            Self::Many { heap, capacity, least, nulls } => {
                if skipped(*nulls, arg, by) {
                    return Ok(());
                }
                insert(heap, *capacity, *least, (by.clone(), arg.clone()))?;
            }
        }
        Ok(())
    }

    /// Folds another state for the same call into this one.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (&mut *self, other) {
            (_, Self::One { set: false, .. }) => {}
            (Self::One { set: false, .. }, other) => other.clone_into(self),
            (Self::One { arg, by, least, .. }, Self::One { arg: theirs, by: their_by, .. }) => {
                let take = match (&*by, their_by) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(by), Some(their_by)) => beats(their_by, by, *least)?,
                };
                if take {
                    (*arg, *by) = (theirs.clone(), their_by.clone());
                }
            }
            (Self::Many { heap, capacity, least, .. }, Self::Many { heap: theirs, .. }) => {
                for pair in theirs {
                    insert(heap, *capacity, *least, pair.clone())?;
                }
            }
            _ => return Err(Error::internal("arg_min combined with a different form of itself")),
        }
        Ok(())
    }

    /// The answer.
    pub(crate) fn finish(&self, returns: &LogicalType) -> Result<Value> {
        match self {
            Self::One { set: false, .. } => Ok(Value::Null),
            Self::One { arg, .. } => Ok(arg.clone()),
            Self::Many { heap, least, .. } => {
                let mut sorted = heap.clone();
                sort_heap(&mut sorted, *least)?;
                let element = match returns {
                    LogicalType::List(element) => (**element).clone(),
                    _ => LogicalType::Null,
                };
                let values = sorted.into_iter().map(|(_, arg)| arg).collect();
                Ok(Value::List { element, values })
            }
        }
    }
}

/// Whether a row is left out altogether. `_nulls_last` leaves nothing out: the list form ranks a
/// null `by` below every other and the single row form handles one apart.
fn skipped(nulls: Nulls, arg: &Value, by: &Value) -> bool {
    match nulls {
        Nulls::Skip => arg.is_null() || by.is_null(),
        Nulls::Arg => by.is_null(),
        Nulls::Last => false,
    }
}

/// The `n` of the list form, checked the way the pin checks it.
fn capacity(n: &Value) -> Result<usize> {
    let invalid =
        |why: &str| Error::invalid_input(format!("Invalid input for arg_min/arg_max: {why}"));
    if n.is_null() {
        return Err(invalid("n value cannot be NULL"));
    }
    let n = integral(n).ok_or_else(|| Error::internal("arg_min with an n that is not a number"))?;
    if n <= 0 {
        return Err(invalid("n value must be > 0"));
    }
    if n >= i128::from(MOST) {
        return Err(invalid(&format!("n value must be < {MOST}")));
    }
    usize::try_from(n).map_err(|_| invalid("n value must be > 0"))
}

/// Whether `by` is strictly better than `kept`, less for a min and greater for a max.
fn beats(by: &Value, kept: &Value, least: bool) -> Result<bool> {
    let ordering = order(by, kept)?;
    Ok(if least { ordering.is_lt() } else { ordering.is_gt() })
}

/// The heap's order, which is the pin's `Compare`: strictly better, with a null `by` worse than
/// any other value whichever way the call looks.
fn better(left: &Value, right: &Value, least: bool) -> Result<bool> {
    Ok(match (left.is_null(), right.is_null()) {
        (true, _) => false,
        (false, true) => true,
        (false, false) => beats(left, right, least)?,
    })
}

/// The pin's `BinaryAggregateHeap::Insert`: fill up to `capacity`, then replace the top only with
/// a row strictly better than it.
fn insert(
    heap: &mut Vec<(Value, Value)>,
    capacity: usize,
    least: bool,
    pair: (Value, Value),
) -> Result<()> {
    if heap.len() < capacity {
        heap.push(pair);
        let last = heap.len() - 1;
        return push_heap(heap, last, 0, least);
    }
    if better(&pair.0, &heap[0].0, least)? {
        let len = heap.len();
        pop_heap(heap, len, least)?;
        heap[len - 1] = pair;
        push_heap(heap, len - 1, 0, least)?;
    }
    Ok(())
}

/// libstdc++'s `__push_heap`: moves the value at `hole` up towards `top` while its parent is before
/// it in the heap's order.
fn push_heap(heap: &mut [(Value, Value)], mut hole: usize, top: usize, least: bool) -> Result<()> {
    while hole > top {
        let parent = (hole - 1) / 2;
        if !better(&heap[parent].0, &heap[hole].0, least)? {
            break;
        }
        heap.swap(parent, hole);
        hole = parent;
    }
    Ok(())
}

/// libstdc++'s `__adjust_heap` from the root over the first `len` slots, with the value to sink
/// already standing at the root.
fn adjust_heap(heap: &mut [(Value, Value)], len: usize, least: bool) -> Result<()> {
    let mut hole = 0;
    let mut child = 0;
    while child < len.saturating_sub(1) / 2 {
        child = 2 * (child + 1);
        if better(&heap[child].0, &heap[child - 1].0, least)? {
            child -= 1;
        }
        heap.swap(hole, child);
        hole = child;
    }
    if len & 1 == 0 && len >= 2 && child == (len - 2) / 2 {
        child = 2 * (child + 1);
        heap.swap(hole, child - 1);
        hole = child - 1;
    }
    push_heap(heap, hole, 0, least)
}

/// libstdc++'s `__pop_heap` over the first `len` slots: the top goes to the last slot and what was
/// there sinks from the root.
fn pop_heap(heap: &mut [(Value, Value)], len: usize, least: bool) -> Result<()> {
    if len > 1 {
        heap.swap(0, len - 1);
        adjust_heap(heap, len - 1, least)?;
    }
    Ok(())
}

/// libstdc++'s `sort_heap`, which leaves the best row first.
fn sort_heap(heap: &mut [(Value, Value)], least: bool) -> Result<()> {
    let mut len = heap.len();
    while len > 1 {
        pop_heap(heap, len, least)?;
        len -= 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, rows: &[(i32, Option<i32>)], n: Option<i64>) -> Value {
        let mut state = ArgExtreme::named(name).unwrap();
        for &(arg, by) in rows {
            let by = by.map_or(Value::Null, Value::Integer);
            let mut args = vec![Value::Integer(arg), by];
            if let Some(n) = n {
                args.push(Value::BigInt(n));
            }
            state.update(&args).unwrap();
        }
        state.finish(&LogicalType::list(LogicalType::Integer)).unwrap()
    }

    fn list(values: &[i32]) -> Value {
        Value::List {
            element: LogicalType::Integer,
            values: values.iter().copied().map(Value::Integer).collect(),
        }
    }

    #[test]
    fn ties_come_out_in_the_order_the_pins_heap_leaves_them() {
        let rows = [(1, 3), (2, 1), (3, 2), (4, 1), (5, 3), (6, 1), (7, 2), (8, 3)]
            .map(|(a, b)| (a, Some(b)));
        assert_eq!(run("arg_min", &rows[..7], Some(5)), list(&[4, 2, 6, 7, 3]));
        assert_eq!(run("arg_max", &rows, Some(4)), list(&[8, 1, 5, 7]));
        let same = [(1, Some(5)), (2, Some(5)), (3, Some(5)), (4, Some(5))];
        assert_eq!(run("arg_max", &same, Some(3)), list(&[3, 2, 1]));
        assert_eq!(run("arg_max", &same, None), Value::Integer(1));
        assert_eq!(run("arg_min", &same, None), Value::Integer(1));
    }

    #[test]
    fn each_spelling_looks_at_the_nulls_it_should() {
        let rows = [(1, None), (2, Some(3)), (3, Some(5))];
        assert_eq!(run("arg_min", &rows, None), Value::Integer(2));
        assert_eq!(run("arg_max_null", &[(1, None), (2, None)], None), Value::Null);
        assert_eq!(run("arg_min_nulls_last", &[(1, None), (2, None)], None), Value::Integer(1));
        assert_eq!(run("arg_min_nulls_last", &[(4, None), (2, Some(9))], Some(2)), list(&[2, 4]));
    }
}

/// A `by` read straight off a typed column, for [`ArgExtreme::cannot_take`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Key {
    Whole(i64),
    Real(f64),
}

impl Key {
    /// The key at `row` of `column`.
    pub(crate) fn at(column: Column<'_>, row: usize) -> Self {
        match column {
            Column::Wholes(_, numbers) => Self::Whole(numbers.at(row)),
            Column::Reals(reals) => Self::Real(reals[row]),
            Column::Flags(flags) => Self::Whole(i64::from(flags[row])),
        }
    }

    /// Whether this key is strictly better than `held`, or `None` when `held` is not a number of
    /// the same kind.
    fn beats(self, held: &Value, least: bool) -> Option<bool> {
        let ordering = match (self, held) {
            (Self::Real(key), Value::Double(held)) => float_order(key, *held),
            (Self::Whole(key), held) => key.cmp(&Whole::of(held)?.1),
            (Self::Real(_), _) => return None,
        };
        Some(if least { ordering.is_lt() } else { ordering.is_gt() })
    }
}
