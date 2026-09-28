//! `lttb(x, y, n)`, Largest Triangle Three Buckets, which thins a series down to `n` points that
//! keep its shape.
//!
//! The points are held in the order they arrive, which is the order the call's `ORDER BY` put them
//! in when it has one, and a combine puts the other state's points after these, which is what the
//! pin's list combine does. Nothing is thinned until the answer is asked for, because the buckets
//! are cut from the count of points and the count is not known before then.
//!
//! The first and last points are always kept. The points between them are cut into `n - 2`
//! buckets of equal width, and each bucket keeps the point that makes the largest triangle with the
//! point kept before it and the average of the next bucket. The arithmetic is the pin's step for
//! step, doubles all the way, so that a tie goes to the same point it goes to there.
//!
//! A call with one `ORDER BY` key keeps the key beside each point, as a number where it is one,
//! and sorts the points by it before thinning them. That is what the pin does with any ordered
//! aggregate, and holding the points and one key here takes a fraction of what holding every row
//! whole for [`crate::general::General::Ordered`] takes. The sort is stable, as that one is, so
//! points that tie on the key keep the order they arrived in. A call with more keys than one goes
//! the general way.
//!
//! A timestamp axis is measured from the first point rather than from the epoch, since a count of
//! nanoseconds since 1970 does not fit in the 53 bits a double holds exactly and moving an axis
//! does not change the area of any triangle.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Validity, Vector};

use crate::compare::{float_order, order_with_nulls};
use crate::number::integral;
use crate::quantile::{Column, Whole};

/// The points of one `lttb` call and the count it thins them to.
#[derive(Debug, Clone)]
pub(crate) struct Plot {
    xs: Axis,
    ys: Axis,
    n: Option<u64>,
    sort: Option<Sort>,
}

/// The one `ORDER BY` key of a call, a key per point.
#[derive(Debug, Clone)]
struct Sort {
    descending: bool,
    nulls_first: bool,
    keys: Keys,
    /// The points whose key was null, in the order they arrived. Their place in `keys` holds a
    /// stand in that is never compared.
    nulls: Vec<usize>,
}

/// The keys of a [`Sort`], as numbers where the key type is held as one. A key column has one type,
/// so a state only moves off [`Keys::Wholes`] while everything in it is a stand in for a null.
#[derive(Debug, Clone)]
enum Keys {
    Wholes(Vec<i64>),
    Reals(Vec<f64>),
    Values(Vec<Value>),
}

impl Sort {
    fn len(&self) -> usize {
        match &self.keys {
            Keys::Wholes(keys) => keys.len(),
            Keys::Reals(keys) => keys.len(),
            Keys::Values(keys) => keys.len(),
        }
    }

    fn push(&mut self, key: &Value) {
        let at = self.len();
        let only_nulls = self.nulls.len() == at;
        let real = match *key {
            Value::Double(real) => Some(real),
            Value::Float(real) => Some(f64::from(real)),
            _ => None,
        };
        // A DECIMAL is left as a value, since a wide one is not always held in an `i64`.
        let whole = match Whole::of(key) {
            Some((Whole::Decimal { .. }, _)) | None => None,
            Some((_, whole)) => Some(whole),
        };
        match (&mut self.keys, key) {
            (Keys::Wholes(keys), Value::Null) => keys.push(0),
            (Keys::Reals(keys), Value::Null) => keys.push(0.0),
            (Keys::Values(keys), Value::Null) => keys.push(Value::Null),
            (Keys::Wholes(keys), _) if whole.is_some() => keys.push(whole.unwrap_or_default()),
            (Keys::Reals(keys), _) if real.is_some() => keys.push(real.unwrap_or_default()),
            (Keys::Wholes(_), _) if only_nulls && real.is_some() => {
                let mut keys = vec![0.0; at];
                keys.push(real.unwrap_or_default());
                self.keys = Keys::Reals(keys);
            }
            (Keys::Values(keys), _) => keys.push(key.clone()),
            (_, _) => {
                let mut keys: Vec<Value> = (0..at).map(|at| self.value(at)).collect();
                keys.push(key.clone());
                self.keys = Keys::Values(keys);
            }
        }
        if key.is_null() {
            self.nulls.push(at);
        }
    }

    fn push_whole(&mut self, key: i64) {
        match &mut self.keys {
            Keys::Wholes(keys) => keys.push(key),
            _ => self.push(&Value::BigInt(key)),
        }
    }

    fn push_real(&mut self, key: f64) {
        match &mut self.keys {
            Keys::Reals(keys) => keys.push(key),
            _ => self.push(&Value::Double(key)),
        }
    }

    /// The key at `at` as a value, which only [`Self::push`] reads back when the keys move to
    /// [`Keys::Values`] or come from another state.
    fn value(&self, at: usize) -> Value {
        if self.nulls.binary_search(&at).is_ok() {
            return Value::Null;
        }
        match &self.keys {
            Keys::Wholes(keys) => Value::BigInt(keys[at]),
            Keys::Reals(keys) => Value::Double(keys[at]),
            Keys::Values(keys) => keys[at].clone(),
        }
    }

    fn extend(&mut self, other: &Self) {
        let at = self.len();
        match (&mut self.keys, &other.keys) {
            (Keys::Wholes(keys), Keys::Wholes(more)) => append(keys, more),
            (Keys::Reals(keys), Keys::Reals(more)) => append(keys, more),
            _ => {
                for index in 0..other.len() {
                    self.push(&other.value(index));
                }
                return;
            }
        }
        self.nulls.extend(other.nulls.iter().map(|index| index + at));
    }

    /// The points in the order the key puts them in.
    fn order(&self) -> Vec<usize> {
        let mut null = vec![false; self.len()];
        for &at in &self.nulls {
            null[at] = true;
        }
        let mut order: Vec<usize> = (0..self.len()).filter(|&at| !null[at]).collect();
        let compared = |a: &usize, b: &usize| {
            let ordering = match &self.keys {
                Keys::Wholes(keys) => keys[*a].cmp(&keys[*b]),
                Keys::Reals(keys) => float_order(keys[*a], keys[*b]),
                Keys::Values(keys) => {
                    order_with_nulls(&keys[*a], &keys[*b], false).unwrap_or(Ordering::Equal)
                }
            };
            if self.descending { ordering.reverse() } else { ordering }
        };
        order.sort_by(compared);
        if self.nulls_first {
            let mut first = self.nulls.clone();
            first.extend(order);
            first
        } else {
            order.extend(self.nulls.iter().copied());
            order
        }
    }
}

/// The columns of a batch of `lttb` rows, read as the numbers they hold so that a row goes in
/// without a value being made for it.
pub(crate) struct Points<'a> {
    x: (Column<'a>, &'a Validity),
    y: (Column<'a>, &'a Validity),
    key: Option<(Column<'a>, &'a Validity)>,
    n: &'a Vector,
}

impl<'a> Points<'a> {
    /// The columns of `args`, the call's arguments and then its one sort key if it has one, or
    /// `None` when one of them is in a form or a type a [`Column`] does not read.
    pub(crate) fn of(args: &'a [Vector], rows: usize) -> Option<Self> {
        let column = |at: usize| {
            let input: &Vector = args.get(at)?;
            Some((Column::of(input, rows)?, input.validity()))
        };
        let key = if args.len() > 3 { Some(column(3)?) } else { None };
        Some(Self { x: column(0)?, y: column(1)?, key, n: args.get(2)? })
    }
}

/// One axis of the points, held in the type it answers in.
#[derive(Debug, Clone)]
enum Axis {
    Float(Vec<f32>),
    Double(Vec<f64>),
    /// Any of the timestamps, in the unit its type counts in, and whether it has a time zone.
    Ticks(Vec<i64>, bool),
}

impl Axis {
    fn of(ty: &LogicalType) -> Self {
        match ty {
            LogicalType::Float => Self::Float(Vec::new()),
            LogicalType::TimestampTz => Self::Ticks(Vec::new(), true),
            LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs => Self::Ticks(Vec::new(), false),
            _ => Self::Double(Vec::new()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Float(values) => values.len(),
            Self::Double(values) => values.len(),
            Self::Ticks(values, _) => values.len(),
        }
    }

    fn push(&mut self, value: &Value) -> Result<()> {
        match (self, value) {
            (Self::Float(values), Value::Float(v)) => values.push(*v),
            (Self::Double(values), Value::Double(v)) => values.push(*v),
            (Self::Ticks(values, _), Value::Timestamp(v) | Value::TimestampTz(v)) => {
                values.push(*v);
            }
            (_, value) => {
                return Err(Error::internal(format!("lttb was handed {value:?} for an axis")));
            }
        }
        Ok(())
    }

    fn extend(&mut self, other: &Self) {
        match (self, other) {
            (Self::Float(values), Self::Float(more)) => append(values, more),
            (Self::Double(values), Self::Double(more)) => append(values, more),
            (Self::Ticks(values, _), Self::Ticks(more, _)) => append(values, more),
            _ => {}
        }
    }

    /// The point at `index` as a double, a timestamp measured from the one at `origin`. The
    /// difference is taken in 128 bits because an infinite timestamp is the least or greatest `i64`.
    #[expect(clippy::cast_precision_loss, reason = "the pin casts the difference to a double")]
    fn at(&self, index: usize, origin: usize) -> f64 {
        match self {
            Self::Float(values) => f64::from(values[index]),
            Self::Double(values) => values[index],
            Self::Ticks(values, _) => {
                (i128::from(values[index]) - i128::from(values[origin])) as f64
            }
        }
    }

    fn value(&self, index: usize) -> Value {
        match self {
            Self::Float(values) => Value::Float(values[index]),
            Self::Double(values) => Value::Double(values[index]),
            Self::Ticks(values, true) => Value::TimestampTz(values[index]),
            Self::Ticks(values, false) => Value::Timestamp(values[index]),
        }
    }
}

impl Plot {
    /// An empty plot for a call that answers `returns`, a list of `(x, y)` structs.
    pub(crate) fn new(returns: &LogicalType) -> Self {
        let point = match returns {
            LogicalType::List(element) => match &**element {
                LogicalType::Struct(fields) => fields.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let axis = |at: usize| point.get(at).map_or(Axis::Double(Vec::new()), |f| Axis::of(&f.ty));
        Self { xs: axis(0), ys: axis(1), n: None, sort: None }
    }

    /// An empty plot for a call with one `ORDER BY` key, whose direction and null placement are
    /// `key`.
    pub(crate) fn sorted(returns: &LogicalType, key: (bool, bool)) -> Self {
        let (descending, nulls_first) = key;
        let keys = Keys::Wholes(Vec::new());
        Self {
            sort: Some(Sort { descending, nulls_first, keys, nulls: Vec::new() }),
            ..Self::new(returns)
        }
    }

    /// Adds a point, and takes the count off `n` the first time. A point with a null axis is
    /// dropped, as the pin drops it. `key` is the row's sort key, which a plot that is not sorted
    /// is not handed.
    pub(crate) fn push(
        &mut self,
        x: &Value,
        y: &Value,
        n: Option<&Value>,
        key: Option<&Value>,
    ) -> Result<()> {
        if x.is_null() || y.is_null() {
            return Ok(());
        }
        if self.n.is_none()
            && let Some(n) = n
        {
            self.n = Some(count(n)?);
        }
        if let Some(sort) = &mut self.sort {
            sort.push(key.unwrap_or(&Value::Null));
        }
        self.xs.push(x)?;
        self.ys.push(y)
    }

    /// Adds the point at `row` of `points`, the way [`Self::push`] adds one made of values.
    pub(crate) fn push_row(&mut self, points: &Points<'_>, row: usize) -> Result<()> {
        let ((x, x_valid), (y, y_valid)) = (points.x, points.y);
        if !x_valid.is_valid(row) || !y_valid.is_valid(row) {
            return Ok(());
        }
        let pushed = match (&mut self.xs, x, &mut self.ys, y) {
            (Axis::Double(xs), Column::Reals(x), Axis::Double(ys), Column::Reals(y)) => {
                xs.push(x[row]);
                ys.push(y[row]);
                true
            }
            (Axis::Ticks(xs, _), Column::Wholes(_, x), Axis::Double(ys), Column::Reals(y)) => {
                xs.push(x.at(row));
                ys.push(y[row]);
                true
            }
            _ => false,
        };
        if !pushed {
            self.xs.push(&x.value(row))?;
            self.ys.push(&y.value(row))?;
        }
        if self.n.is_none() {
            self.n = Some(count(&points.n.try_value_at(row)?)?);
        }
        if let Some(sort) = &mut self.sort {
            match points.key {
                Some((_, valid)) if !valid.is_valid(row) => sort.push(&Value::Null),
                Some((Column::Reals(keys), _)) => sort.push_real(keys[row]),
                Some((Column::Wholes(whole, keys), _))
                    if !matches!(whole, Whole::Decimal { .. }) =>
                {
                    sort.push_whole(keys.at(row));
                }
                Some((column, _)) => sort.push(&column.value(row)),
                None => sort.push(&Value::Null),
            }
        }
        Ok(())
    }

    /// Puts the points of `other` after these.
    pub(crate) fn combine(&mut self, other: &Self) {
        self.xs.extend(&other.xs);
        self.ys.extend(&other.ys);
        if let (Some(sort), Some(theirs)) = (&mut self.sort, &other.sort) {
            sort.extend(theirs);
        }
        if self.n.is_none() {
            self.n = other.n;
        }
    }

    /// The kept points as a list of `(x, y)` structs, or a null when there are none.
    pub(crate) fn finish(&self, returns: &LogicalType) -> Value {
        let v = self.xs.len();
        if v == 0 {
            return Value::Null;
        }
        let n = self.n.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX);
        let order = self.sort.as_ref().map_or_else(|| (0..v).collect(), Sort::order);
        let kept: Vec<usize> = if v <= n {
            order
        } else if n < 3 {
            vec![order[0], order[v - 1]]
        } else {
            self.thin(&order, n)
        };
        let values = kept
            .into_iter()
            .map(|at| {
                let x = ("x".to_string(), self.xs.value(at));
                Value::Struct(vec![x, ("y".to_string(), self.ys.value(at))])
            })
            .collect();
        let element = match returns {
            LogicalType::List(element) => (**element).clone(),
            _ => LogicalType::Null,
        };
        Value::List { element, values }
    }

    /// The indices of the `n` points kept out of the ones `order` lists, for `2 < n` and fewer
    /// than `order` holds.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the pin does the bucket arithmetic in doubles and floors back to an index"
    )]
    fn thin(&self, order: &[usize], n: usize) -> Vec<usize> {
        let v = order.len();
        let origin = order[0];
        let x = |at: usize| self.xs.at(order[at], origin);
        let y = |at: usize| self.ys.at(order[at], origin);
        let width = (v - 2) as f64 / (n - 2) as f64;
        let bucket = |at: usize| (at as f64 * width).floor() as usize + 1;
        let mut kept = Vec::with_capacity(n);
        kept.push(order[0]);
        let mut previous = 0;
        for i in 1..n - 1 {
            let (next_start, next_end) = (bucket(i), bucket(i + 1).min(v));
            let (start, end) = (bucket(i - 1), bucket(i).min(v));
            let (mut bx, mut by) = (0.0, 0.0);
            for j in next_start..next_end {
                bx += x(j);
                by += y(j);
            }
            let len = (next_end - next_start) as f64;
            bx /= len;
            by /= len;
            let (ax, ay) = (x(previous), y(previous));
            let (mut best, mut most) = (start, -1.0);
            for c in start..end {
                let (cx, cy) = (x(c), y(c));
                let area = ((ax - bx) * (cy - ay) - (ax - cx) * (by - ay)).abs() * 0.5;
                if area > most {
                    most = area;
                    best = c;
                }
            }
            kept.push(order[best]);
            previous = best;
        }
        kept.push(order[v - 1]);
        kept
    }
}

/// Puts `more` after `values` without the room a push leaves for the next one, since a combine is
/// the last thing that grows a state and a grouped query holds a state per group.
fn append<T: Copy>(values: &mut Vec<T>, more: &[T]) {
    values.reserve_exact(more.len());
    values.extend_from_slice(more);
}

/// The count of points a call asked for, which the binder has already checked is at least 2.
fn count(n: &Value) -> Result<u64> {
    integral(n)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| Error::internal(format!("lttb was handed {n:?} for the number of points")))
}

#[cfg(test)]
mod tests {
    use rudb_common::Field;

    use super::*;

    fn plot(x: &LogicalType) -> (Plot, LogicalType) {
        let point = vec![Field::new("x", x.clone()), Field::new("y", LogicalType::Double)];
        let returns = LogicalType::list(LogicalType::Struct(point));
        (Plot::new(&returns), returns)
    }

    fn xs(answer: &Value) -> Vec<Value> {
        let Value::List { values, .. } = answer else { panic!("not a list: {answer:?}") };
        values
            .iter()
            .map(|point| match point {
                Value::Struct(fields) => fields[0].1.clone(),
                _ => panic!("not a point: {point:?}"),
            })
            .collect()
    }

    #[test]
    fn a_parabola_thins_to_the_points_the_pin_keeps() {
        for (n, want) in [(5, vec![0.0, 2.0, 5.0, 7.0, 9.0]), (4, vec![0.0, 3.0, 6.0, 9.0])] {
            let (mut plot, returns) = plot(&LogicalType::Double);
            for x in 0..10 {
                let (x, y) = (Value::Double(f64::from(x)), Value::Double(f64::from(x * x)));
                plot.push(&x, &y, Some(&Value::BigInt(n)), None).expect("pushed");
            }
            let answer = plot.finish(&returns);
            let want: Vec<Value> = want.into_iter().map(Value::Double).collect();
            assert_eq!(xs(&answer), want, "n = {n}");
        }
    }

    #[test]
    fn a_sorted_plot_thins_the_points_in_key_order() {
        let (_, returns) = plot(&LogicalType::Double);
        for key in [(false, false), (true, true)] {
            let mut plot = Plot::sorted(&returns, key);
            // The points arrive shuffled, with a null key that goes last ascending and first
            // descending, and a point that is dropped for its null y.
            for x in [7_i32, 2, 9, 0, 4, 1, 8, 3, 6, 5] {
                let (key, y) = (Value::BigInt(i64::from(x)), Value::Double(f64::from(x * x)));
                plot.push(&Value::Double(f64::from(x)), &y, Some(&Value::BigInt(5)), Some(&key))
                    .expect("pushed");
            }
            let n = Some(&Value::BigInt(5));
            plot.push(&Value::Double(-1.0), &Value::Null, n, Some(&Value::BigInt(3)))
                .expect("pushed");
            plot.push(&Value::Double(10.0), &Value::Double(0.0), n, Some(&Value::Null))
                .expect("pushed");
            let answer = xs(&plot.finish(&returns));
            let want = if key.0 { [10.0, 9.0, 5.0, 3.0, 0.0] } else { [0.0, 3.0, 6.0, 9.0, 10.0] };
            assert_eq!(answer, want.map(Value::Double), "{key:?}");
        }
    }

    #[test]
    fn nulls_are_dropped_and_nothing_left_is_null() {
        let (mut plot, returns) = plot(&LogicalType::Double);
        plot.push(&Value::Null, &Value::Double(1.0), Some(&Value::BigInt(3)), None)
            .expect("pushed");
        plot.push(&Value::Double(1.0), &Value::Null, Some(&Value::BigInt(3)), None)
            .expect("pushed");
        assert_eq!(plot.finish(&returns), Value::Null);
    }

    #[test]
    fn an_infinite_timestamp_is_measured_without_overflow() {
        let (mut plot, returns) = plot(&LogicalType::Timestamp);
        let day = 86_400_000_000;
        let mut points = vec![(Value::Timestamp(i64::MIN), 0.0)];
        for at in 1..6 {
            #[expect(clippy::cast_precision_loss, reason = "small counts")]
            points.push((Value::Timestamp(at * day), at as f64));
        }
        for (x, y) in &points {
            plot.push(x, &Value::Double(*y), Some(&Value::BigInt(3)), None).expect("pushed");
        }
        let answer = xs(&plot.finish(&returns));
        assert_eq!(
            answer,
            [Value::Timestamp(i64::MIN), Value::Timestamp(day), Value::Timestamp(5 * day)]
        );
    }
}
