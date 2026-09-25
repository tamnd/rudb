//! The aggregates that need every value before they can answer: `quantile_cont`, `quantile_disc`,
//! `median`, `mad` and `mode`.
//!
//! The values of a group are held as they arrive, nulls left out, in a [`Held`] that keeps a column
//! of numbers as the numbers and not as a `Value` each. The answer is found with a selection rather
//! than a sort, which puts one value in its place and leaves the rest in no particular order, and
//! is linear where a sort is not. Every rule below is the pin's, read off its answers rather than
//! assumed.
//!
//! A discrete quantile is a value of the group. Its position is worked out the way the pin works
//! it out, as `max(1, n - floor(n - n * q)) - 1`, and when the fraction was written as a decimal
//! that sum is done in whole numbers, so `quantile_disc(x, 0.1)` over ten rows is the first row and
//! not the second one a double would round to.
//!
//! A continuous quantile sits between the two values either side of `(n - 1) * q` and is
//! `lo * (1 - d) + hi * d`, where `d` is how far along it is. That is the pin's arithmetic to the
//! last digit, so a BIGINT quantile of 0.3 between 1 and 2 is 1.2999999999999998. A decimal answer
//! is truncated back to its scale, and a time is rounded to the microsecond.
//!
//! A negative fraction reads the values from the top, which is how `ORDER BY x DESC` inside the
//! call reaches the aggregate. `median` is the continuous 0.5 quantile over anything that can be
//! interpolated and the discrete one over anything else. `mad` is the median distance from the
//! median. `mode` is the value seen most often, and of those the one seen first.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, Vector};

use crate::compare::{float_order, order};
use crate::number::{approximate, pow10};

/// Which answer a [`crate::general::General::Holistic`] state gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holistic {
    Continuous,
    Discrete,
    Median,
    Deviation,
    Mode,
}

impl Holistic {
    /// The aggregate a name is, or `None` when it is not one of these.
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "quantile_cont" => Self::Continuous,
            "quantile_disc" => Self::Discrete,
            "median" => Self::Median,
            "mad" => Self::Deviation,
            "mode" => Self::Mode,
            _ => return None,
        })
    }
}

/// The values of one group, in the narrowest form their type allows.
///
/// A median over ten million rows was a `Value` of 32 bytes a row and a sort that called the
/// general comparison for every pair it looked at, which is where 4 GB and 14 seconds went. Any
/// type that is a whole number underneath, a decimal that fits one included, is held as `i64`
/// and a DOUBLE as `f64`. Anything else, or a group whose values stop fitting, is held as values.
#[derive(Debug, Clone, Default)]
pub(crate) enum Held {
    #[default]
    Empty,
    Wholes {
        values: Vec<i64>,
        whole: Whole,
    },
    Reals(Vec<f64>),
    Values(Vec<Value>),
}

/// Which type a [`Held::Wholes`] is a column of, to turn a number back into its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Whole {
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    UTinyInt,
    USmallInt,
    UInteger,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Decimal { width: u8, scale: u8 },
}

impl Whole {
    /// The number a value is, and which type it came from, or `None` for a value that is not held
    /// as a number.
    pub(crate) fn of(value: &Value) -> Option<(Self, i64)> {
        Some(match *value {
            Value::TinyInt(n) => (Self::TinyInt, i64::from(n)),
            Value::SmallInt(n) => (Self::SmallInt, i64::from(n)),
            Value::Integer(n) => (Self::Integer, i64::from(n)),
            Value::BigInt(n) => (Self::BigInt, n),
            Value::UTinyInt(n) => (Self::UTinyInt, i64::from(n)),
            Value::USmallInt(n) => (Self::USmallInt, i64::from(n)),
            Value::UInteger(n) => (Self::UInteger, i64::from(n)),
            Value::Date(n) => (Self::Date, i64::from(n)),
            Value::Time(n) => (Self::Time, n),
            Value::Timestamp(n) => (Self::Timestamp, n),
            Value::TimestampTz(n) => (Self::TimestampTz, n),
            Value::Decimal { unscaled, width, scale } => {
                (Self::Decimal { width, scale }, i64::try_from(unscaled).ok()?)
            }
            _ => return None,
        })
    }

    /// The value a number held as this type is.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "every number was widened from this type, so it narrows back without loss"
    )]
    fn value(self, n: i64) -> Value {
        match self {
            Self::TinyInt => Value::TinyInt(n as i8),
            Self::SmallInt => Value::SmallInt(n as i16),
            Self::Integer => Value::Integer(n as i32),
            Self::BigInt => Value::BigInt(n),
            Self::UTinyInt => Value::UTinyInt(n as u8),
            Self::USmallInt => Value::USmallInt(n as u16),
            Self::UInteger => Value::UInteger(n as u32),
            Self::Date => Value::Date(n as i32),
            Self::Time => Value::Time(n),
            Self::Timestamp => Value::Timestamp(n),
            Self::TimestampTz => Value::TimestampTz(n),
            Self::Decimal { width, scale } => {
                Value::Decimal { unscaled: i128::from(n), width, scale }
            }
        }
    }

    /// Whether `mad` over this type answers in an interval of microseconds.
    fn timed(self) -> bool {
        matches!(self, Self::Time | Self::Timestamp | Self::TimestampTz)
    }
}

impl Held {
    /// Adds a value that is not null.
    pub(crate) fn push(&mut self, value: &Value) {
        match self {
            Self::Empty => {
                *self = match (Whole::of(value), value) {
                    (Some((whole, n)), _) => Self::Wholes { values: vec![n], whole },
                    (None, Value::Double(real)) => Self::Reals(vec![*real]),
                    (None, value) => Self::Values(vec![value.clone()]),
                };
            }
            Self::Wholes { values, whole } => match Whole::of(value) {
                Some((kind, n)) if kind == *whole => values.push(n),
                _ => self.spilled().push(value.clone()),
            },
            Self::Reals(values) => match value {
                Value::Double(real) => values.push(*real),
                _ => self.spilled().push(value.clone()),
            },
            Self::Values(values) => values.push(value.clone()),
        }
    }

    /// Adds a whole number of one type, which is how a column is read without a value a row.
    fn push_whole(&mut self, whole: Whole, n: i64) {
        match self {
            Self::Wholes { values, whole: held } if *held == whole => values.push(n),
            Self::Empty => *self = Self::Wholes { values: vec![n], whole },
            _ => self.spilled().push(whole.value(n)),
        }
    }

    /// Adds a double.
    fn push_real(&mut self, real: f64) {
        match self {
            Self::Reals(values) => values.push(real),
            Self::Empty => *self = Self::Reals(vec![real]),
            _ => self.spilled().push(Value::Double(real)),
        }
    }

    /// Adds what another group of the same call holds after what this one holds.
    pub(crate) fn append(&mut self, other: &Self) {
        match (&mut *self, other) {
            (_, Self::Empty) => {}
            (Self::Empty, other) => other.clone_into(self),
            (Self::Wholes { values, whole }, Self::Wholes { values: more, whole: theirs })
                if whole == theirs =>
            {
                values.extend_from_slice(more);
            }
            (Self::Reals(values), Self::Reals(more)) => values.extend_from_slice(more),
            (held, other) => {
                let more = other.values();
                held.spilled().extend(more);
            }
        }
    }

    /// The values, one `Value` each.
    fn values(&self) -> Vec<Value> {
        match self {
            Self::Empty => Vec::new(),
            Self::Wholes { values, whole } => values.iter().map(|&n| whole.value(n)).collect(),
            Self::Reals(values) => values.iter().copied().map(Value::Double).collect(),
            Self::Values(values) => values.clone(),
        }
    }

    /// Turns this into [`Held::Values`] and hands back the values to add to.
    fn spilled(&mut self) -> &mut Vec<Value> {
        if !matches!(self, Self::Values(_)) {
            *self = Self::Values(self.values());
        }
        match self {
            Self::Values(values) => values,
            _ => unreachable!("a held set of values was just made one"),
        }
    }
}

/// A flat column of numbers read where it lies, for a [`Held`] to take a row at a time without a
/// `Value` made for it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Column<'a> {
    Wholes(Whole, Numbers<'a>),
    Reals(&'a [f64]),
}

/// The numbers of a [`Column::Wholes`], in whatever width the vector stores them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Numbers<'a> {
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
}

impl Numbers<'_> {
    /// The number at `row`, widened.
    pub(crate) fn at(self, row: usize) -> i64 {
        match self {
            Self::I8(n) => i64::from(n[row]),
            Self::I16(n) => i64::from(n[row]),
            Self::I32(n) => i64::from(n[row]),
            Self::I64(n) => n[row],
            Self::U8(n) => i64::from(n[row]),
            Self::U16(n) => i64::from(n[row]),
            Self::U32(n) => i64::from(n[row]),
        }
    }
}

impl<'a> Column<'a> {
    /// The column a flat vector of `rows` rows is, or `None` for a form or a type this does not
    /// read, which then goes in a value at a time.
    pub(crate) fn of(input: &'a Vector, rows: usize) -> Option<Self> {
        if input.form() != Form::Flat {
            return None;
        }
        let data = input.data()?;
        let ty = input.logical_type();
        if let (LogicalType::Double, Data::Float64(reals)) = (ty, data) {
            return reals.get(..rows).map(Self::Reals);
        }
        let whole = match *ty {
            LogicalType::TinyInt => Whole::TinyInt,
            LogicalType::SmallInt => Whole::SmallInt,
            LogicalType::Integer => Whole::Integer,
            LogicalType::BigInt => Whole::BigInt,
            LogicalType::UTinyInt => Whole::UTinyInt,
            LogicalType::USmallInt => Whole::USmallInt,
            LogicalType::UInteger => Whole::UInteger,
            LogicalType::Date => Whole::Date,
            LogicalType::Time => Whole::Time,
            LogicalType::Timestamp => Whole::Timestamp,
            LogicalType::TimestampTz => Whole::TimestampTz,
            LogicalType::Decimal { width, scale } => Whole::Decimal { width, scale },
            _ => return None,
        };
        let numbers = match data {
            Data::Int8(n) => Numbers::I8(n.get(..rows)?),
            Data::Int16(n) => Numbers::I16(n.get(..rows)?),
            Data::Int32(n) => Numbers::I32(n.get(..rows)?),
            Data::Int64(n) => Numbers::I64(n.get(..rows)?),
            Data::UInt8(n) => Numbers::U8(n.get(..rows)?),
            Data::UInt16(n) => Numbers::U16(n.get(..rows)?),
            Data::UInt32(n) => Numbers::U32(n.get(..rows)?),
            _ => return None,
        };
        Some(Self::Wholes(whole, numbers))
    }

    /// Adds the value at `row` to `held`.
    pub(crate) fn push(self, held: &mut Held, row: usize) {
        match self {
            Self::Reals(reals) => held.push_real(reals[row]),
            Self::Wholes(whole, numbers) => {
                held.push_whole(whole, numbers.at(row));
            }
        }
    }
}

/// One fraction of a quantile call.
#[derive(Debug, Clone, Copy)]
struct Fraction {
    /// How far through the values, from 0 to 1.
    share: f64,
    /// The fraction as a whole number over a power of ten, when it was written as a decimal.
    exact: Option<(i128, i128)>,
    /// Counted from the top rather than the bottom.
    descending: bool,
}

impl Fraction {
    const HALF: Self = Self { share: 0.5, exact: None, descending: false };

    fn of(value: &Value) -> Result<Self> {
        if let Value::Decimal { unscaled, scale, .. } = *value {
            let scaling = pow10(scale);
            let whole = unscaled.abs();
            #[expect(
                clippy::cast_precision_loss,
                reason = "a fraction between -1 and 1 is a few digits over a power of ten"
            )]
            let share = whole as f64 / scaling as f64;
            return Ok(Self { share, exact: Some((whole, scaling)), descending: unscaled < 0 });
        }
        let share = approximate(value)
            .ok_or_else(|| Error::internal(format!("a quantile of {}", value.logical_type())))?;
        Ok(Self { share: share.abs(), exact: None, descending: share < 0.0 })
    }

    /// Where a discrete quantile sits among `n` sorted values, counted from the bottom.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "the position is between 0 and n and n is a count of rows held in memory"
    )]
    fn discrete(self, n: usize) -> usize {
        let count = n as i128;
        let floored = match self.exact {
            Some((whole, scaling)) => (count * scaling - count * whole) / scaling,
            None => (n as f64 - n as f64 * self.share).floor() as i128,
        };
        let at = ((count - floored).max(1) - 1) as usize;
        self.placed(at.min(n - 1), n)
    }

    /// The two positions a continuous quantile sits between and how far along from the first.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        reason = "the position is between 0 and n and n is a count of rows held in memory"
    )]
    fn continuous(self, n: usize) -> (usize, usize, f64) {
        let row = (n - 1) as f64 * self.share;
        let below = row.floor();
        let above = row.ceil();
        let (low, high) = ((below as usize).min(n - 1), (above as usize).min(n - 1));
        (self.placed(low, n), self.placed(high, n), row - below)
    }

    fn placed(self, at: usize, n: usize) -> usize {
        if self.descending { n - 1 - at } else { at }
    }
}

/// The answer over the values of a group, which are not null.
pub(crate) fn finish(
    measure: Holistic,
    held: &Held,
    fraction: Option<&Value>,
    returns: &LogicalType,
) -> Result<Value> {
    match held {
        Held::Empty => Ok(Value::Null),
        Held::Wholes { values, whole } => {
            let whole = *whole;
            if measure == Holistic::Mode {
                return Ok(whole.value(typed_mode(values, Ord::cmp)));
            }
            let mut numbers = values.clone();
            let deviation = |numbers: &mut [i64]| {
                let middle = continuous(&typed_pair(numbers, Fraction::HALF, Ord::cmp), whole)?;
                let middle = Whole::of(&middle).map_or(0, |(_, n)| n);
                for n in numbers.iter_mut() {
                    *n = n.abs_diff(middle).cast_signed();
                }
                let (low, high, along) = typed_pair(numbers, Fraction::HALF, Ord::cmp);
                if whole.timed() {
                    return Ok(span(rounded(mixed(low, high, along))));
                }
                continuous(&(low, high, along), whole)
            };
            typed(measure, &mut numbers, fraction, returns, Ord::cmp, whole, deviation)
        }
        Held::Reals(values) => {
            let cmp = |left: &f64, right: &f64| float_order(*left, *right);
            if measure == Holistic::Mode {
                return Ok(Value::Double(typed_mode(values, cmp)));
            }
            let mut numbers = values.clone();
            let deviation = |numbers: &mut [f64]| {
                let (low, high, along) = typed_pair(numbers, Fraction::HALF, cmp);
                let middle = mixed(low, high, along);
                for n in numbers.iter_mut() {
                    *n = (*n - middle).abs();
                }
                let (low, high, along) = typed_pair(numbers, Fraction::HALF, cmp);
                Ok(Value::Double(mixed(low, high, along)))
            };
            typed(measure, &mut numbers, fraction, returns, cmp, Real, deviation)
        }
        Held::Values(values) => finish_values(measure, values, fraction, returns),
    }
}

/// A number held in a [`Held`], which knows how to be mixed and turned back into a value.
trait Number: Copy {
    fn real(self) -> f64;
}

impl Number for i64 {
    #[expect(clippy::cast_precision_loss, reason = "the pin mixes in doubles too")]
    fn real(self) -> f64 {
        self as f64
    }
}

impl Number for f64 {
    fn real(self) -> f64 {
        self
    }
}

/// How the numbers of a [`Held`] become values again.
trait Rebuild<T>: Copy {
    fn value(self, n: T) -> Value;
    /// The value `along` of the way from `lo` to `hi`.
    fn mix(self, lo: T, hi: T, along: f64) -> Result<Value>;
}

impl Rebuild<i64> for Whole {
    fn value(self, n: i64) -> Value {
        Whole::value(self, n)
    }

    fn mix(self, lo: i64, hi: i64, along: f64) -> Result<Value> {
        interpolate(&self.value(lo), &self.value(hi), along)
    }
}

/// The doubles of a [`Held::Reals`].
#[derive(Clone, Copy)]
struct Real;

impl Rebuild<f64> for Real {
    fn value(self, n: f64) -> Value {
        Value::Double(n)
    }

    fn mix(self, lo: f64, hi: f64, along: f64) -> Result<Value> {
        Ok(Value::Double(mixed(lo, hi, along)))
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a distance between two times in range is well inside an i64 of microseconds"
)]
fn rounded(micros: f64) -> i64 {
    micros.round() as i64
}

fn mixed<T: Number>(lo: T, hi: T, along: f64) -> f64 {
    lo.real() * (1.0 - along) + hi.real() * along
}

/// The quantiles and the median over numbers, found by selection.
fn typed<T: Number, R: Rebuild<T>>(
    measure: Holistic,
    numbers: &mut [T],
    fraction: Option<&Value>,
    returns: &LogicalType,
    cmp: impl Fn(&T, &T) -> Ordering + Copy,
    rebuild: R,
    deviation: impl Fn(&mut [T]) -> Result<Value>,
) -> Result<Value> {
    let one = |numbers: &mut [T], fraction: Fraction, discrete: bool| {
        if discrete {
            let at = fraction.discrete(numbers.len());
            Ok(rebuild.value(*numbers.select_nth_unstable_by(at, cmp).1))
        } else {
            continuous(&typed_pair(numbers, fraction, cmp), rebuild)
        }
    };
    match measure {
        Holistic::Continuous | Holistic::Discrete => {
            let fraction =
                fraction.ok_or_else(|| Error::internal("a quantile with no fraction"))?;
            let discrete = measure == Holistic::Discrete;
            match (fraction, returns) {
                (Value::List { values: fractions, .. }, LogicalType::List(element)) => {
                    let answers = fractions
                        .iter()
                        .map(|fraction| one(numbers, Fraction::of(fraction)?, discrete))
                        .collect::<Result<Vec<Value>>>()?;
                    Ok(Value::List { element: (**element).clone(), values: answers })
                }
                (fraction, _) => one(numbers, Fraction::of(fraction)?, discrete),
            }
        }
        Holistic::Median => one(numbers, Fraction::HALF, !interpolates(returns)),
        Holistic::Deviation => deviation(numbers),
        Holistic::Mode => Err(Error::internal("mode answered by selection")),
    }
}

/// The two values a continuous quantile sits between and how far along from the first, put in
/// their places by selection. The two positions are next to each other or the same one, so the
/// second is the least of what the first selection left above the first.
fn typed_pair<T: Copy>(
    numbers: &mut [T],
    fraction: Fraction,
    cmp: impl Fn(&T, &T) -> Ordering + Copy,
) -> (T, T, f64) {
    let (low, high, along) = fraction.continuous(numbers.len());
    let (first, second) = (low.min(high), low.max(high));
    let (_, &mut at, above) = numbers.select_nth_unstable_by(first, cmp);
    let next = if second == first {
        at
    } else {
        *above.iter().min_by(|left, right| cmp(left, right)).unwrap_or(&at)
    };
    if low <= high { (at, next, along) } else { (next, at, along) }
}

fn continuous<T: Copy, R: Rebuild<T>>(pair: &(T, T, f64), rebuild: R) -> Result<Value> {
    let &(low, high, along) = pair;
    if along == 0.0 {
        return Ok(rebuild.value(low));
    }
    rebuild.mix(low, high, along)
}

/// The number seen most often, and of those the one seen first.
fn typed_mode<T: Copy>(numbers: &[T], cmp: impl Fn(&T, &T) -> Ordering) -> T {
    let mut seen: Vec<(T, usize)> = numbers.iter().copied().zip(0..).collect();
    seen.sort_unstable_by(|left, right| cmp(&left.0, &right.0).then(left.1.cmp(&right.1)));
    let mut best = (0, usize::MAX);
    let mut start = 0;
    while start < seen.len() {
        let mut end = start + 1;
        while end < seen.len() && cmp(&seen[end].0, &seen[start].0).is_eq() {
            end += 1;
        }
        let (count, first) = (end - start, seen[start].1);
        if count > best.0 || (count == best.0 && first < best.1) {
            best = (count, first);
        }
        start = end;
    }
    numbers[best.1]
}

/// The answer over values that are not held as numbers, sorted in full.
fn finish_values(
    measure: Holistic,
    values: &[Value],
    fraction: Option<&Value>,
    returns: &LogicalType,
) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    if measure == Holistic::Mode {
        return mode(values);
    }
    let sorted = sorted(values)?;
    match measure {
        Holistic::Continuous | Holistic::Discrete => {
            let fraction =
                fraction.ok_or_else(|| Error::internal("a quantile with no fraction"))?;
            let one = |fraction: Fraction| {
                if measure == Holistic::Discrete {
                    Ok(sorted[fraction.discrete(sorted.len())].clone())
                } else {
                    sorted_continuous(&sorted, fraction)
                }
            };
            match (fraction, returns) {
                (Value::List { values: fractions, .. }, LogicalType::List(element)) => {
                    let answers = fractions
                        .iter()
                        .map(|fraction| one(Fraction::of(fraction)?))
                        .collect::<Result<Vec<Value>>>()?;
                    Ok(Value::List { element: (**element).clone(), values: answers })
                }
                (fraction, _) => one(Fraction::of(fraction)?),
            }
        }
        Holistic::Median if interpolates(returns) => sorted_continuous(&sorted, Fraction::HALF),
        Holistic::Median => Ok(sorted[Fraction::HALF.discrete(sorted.len())].clone()),
        Holistic::Deviation => {
            let middle = sorted_continuous(&sorted, Fraction::HALF)?;
            let distances = sorted
                .iter()
                .map(|value| distance(value, &middle))
                .collect::<Result<Vec<Value>>>()?;
            sorted_continuous(&self::sorted(&distances)?, Fraction::HALF)
        }
        Holistic::Mode => mode(values),
    }
}

/// Whether `median` over a type interpolates rather than picking a value.
fn interpolates(returns: &LogicalType) -> bool {
    matches!(
        returns,
        LogicalType::Double
            | LogicalType::Float
            | LogicalType::Decimal { .. }
            | LogicalType::Timestamp
            | LogicalType::TimestampTz
            | LogicalType::Time
            | LogicalType::Interval
    )
}

fn sorted(values: &[Value]) -> Result<Vec<Value>> {
    let mut failure = None;
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| {
        order(left, right).unwrap_or_else(|error| {
            failure.get_or_insert(error);
            Ordering::Equal
        })
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(sorted),
    }
}

fn sorted_continuous(sorted: &[Value], fraction: Fraction) -> Result<Value> {
    let (low, high, along) = fraction.continuous(sorted.len());
    if low == high {
        return Ok(sorted[low].clone());
    }
    interpolate(&sorted[low], &sorted[high], along)
}

/// The value `along` of the way from `lo` to `hi`, in the pin's arithmetic.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the answer lies between two values of the same type, so it fits that type"
)]
fn interpolate(lo: &Value, hi: &Value, along: f64) -> Result<Value> {
    let mix = |lo: f64, hi: f64| lo * (1.0 - along) + hi * along;
    Ok(match (lo, hi) {
        (Value::Double(lo), Value::Double(hi)) => Value::Double(mix(*lo, *hi)),
        (Value::Float(lo), Value::Float(hi)) => {
            Value::Float(mix(f64::from(*lo), f64::from(*hi)) as f32)
        }
        (Value::Decimal { unscaled: lo, width, scale }, Value::Decimal { unscaled: hi, .. }) => {
            Value::Decimal {
                unscaled: mix(*lo as f64, *hi as f64).trunc() as i128,
                width: *width,
                scale: *scale,
            }
        }
        (Value::Timestamp(lo), Value::Timestamp(hi)) => {
            Value::Timestamp(mix(*lo as f64, *hi as f64).round() as i64)
        }
        (Value::TimestampTz(lo), Value::TimestampTz(hi)) => {
            Value::TimestampTz(mix(*lo as f64, *hi as f64).round() as i64)
        }
        (Value::Time(lo), Value::Time(hi)) => {
            Value::Time(mix(*lo as f64, *hi as f64).round() as i64)
        }
        // An interval is mixed as one length, a month as thirty days, and comes back as days and
        // microseconds, which keeps half of a day and a half from rounding to a whole one.
        (lo @ Value::Interval { .. }, hi @ Value::Interval { .. }) => {
            span(mix(length(lo) as f64, length(hi) as f64).round() as i64)
        }
        (lo, _) => {
            return Err(Error::internal(format!("a continuous quantile of {}", lo.logical_type())));
        }
    })
}

const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;

/// An interval as microseconds, a month as thirty days.
fn length(value: &Value) -> i64 {
    match *value {
        Value::Interval { months, days, micros } => {
            (i64::from(months) * 30 + i64::from(days)) * MICROS_PER_DAY + micros
        }
        _ => 0,
    }
}

/// Microseconds as an interval of days and microseconds.
fn span(micros: i64) -> Value {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a distance between two times in range is well inside an INTEGER of days"
    )]
    let days = (micros / MICROS_PER_DAY) as i32;
    Value::Interval { months: 0, days, micros: micros % MICROS_PER_DAY }
}

/// How far a value is from the median, in the type `mad` answers in.
fn distance(value: &Value, middle: &Value) -> Result<Value> {
    Ok(match (value, middle) {
        (Value::Double(value), Value::Double(middle)) => Value::Double((value - middle).abs()),
        (Value::Float(value), Value::Float(middle)) => Value::Float((value - middle).abs()),
        (Value::Decimal { unscaled, width, scale }, Value::Decimal { unscaled: middle, .. }) => {
            Value::Decimal { unscaled: (unscaled - middle).abs(), width: *width, scale: *scale }
        }
        (Value::Timestamp(value), Value::Timestamp(middle))
        | (Value::TimestampTz(value), Value::TimestampTz(middle))
        | (Value::Time(value), Value::Time(middle)) => span((value - middle).abs()),
        (value, _) => {
            return Err(Error::internal(format!("mad of {}", value.logical_type())));
        }
    })
}

/// The value seen most often, and of those the one seen first.
fn mode(values: &[Value]) -> Result<Value> {
    let mut failure = None;
    let mut positions: Vec<usize> = (0..values.len()).collect();
    // Stable, so each run of equal values starts with the one that arrived first.
    positions.sort_by(|&left, &right| {
        order(&values[left], &values[right]).unwrap_or_else(|error| {
            failure.get_or_insert(error);
            Ordering::Equal
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let mut best: Option<(usize, usize)> = None;
    let mut start = 0;
    while start < positions.len() {
        let first = positions[start];
        let mut end = start + 1;
        while end < positions.len() && order(&values[positions[end]], &values[first])?.is_eq() {
            end += 1;
        }
        let count = end - start;
        let better = match best {
            None => true,
            Some((held, at)) => count > held || (count == held && first < at),
        };
        if better {
            best = Some((count, first));
        }
        start = end;
    }
    Ok(best.map_or(Value::Null, |(_, at)| values[at].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(values: &[i64]) -> Vec<Value> {
        values.iter().map(|&value| Value::Double(value as f64)).collect()
    }

    fn held(values: &[Value]) -> Held {
        let mut held = Held::Empty;
        for value in values {
            held.push(value);
        }
        held
    }

    fn decimal(unscaled: i128, scale: u8) -> Value {
        Value::Decimal { unscaled, width: 3, scale }
    }

    #[test]
    fn a_discrete_quantile_over_a_decimal_fraction_counts_in_whole_numbers() {
        let values: Vec<Value> = (0..10).map(Value::BigInt).collect();
        let disc = |fraction: Value| {
            finish(Holistic::Discrete, &held(&values), Some(&fraction), &LogicalType::BigInt)
                .unwrap()
        };
        assert_eq!(disc(decimal(1, 1)), Value::BigInt(0));
        assert_eq!(disc(Value::Float(0.1)), Value::BigInt(1));
        assert_eq!(disc(decimal(25, 2)), Value::BigInt(2));
        assert_eq!(disc(decimal(-25, 2)), Value::BigInt(7));
        assert_eq!(disc(decimal(99, 2)), Value::BigInt(9));
        assert_eq!(disc(Value::Integer(1)), Value::BigInt(9));
        assert_eq!(disc(Value::Integer(0)), Value::BigInt(0));
    }

    #[test]
    fn a_continuous_quantile_mixes_the_two_values_either_side() {
        let cont = |values: &[Value], fraction: Value| {
            finish(Holistic::Continuous, &held(values), Some(&fraction), &LogicalType::Double)
                .unwrap()
        };
        assert_eq!(cont(&whole(&[1, 2]), decimal(3, 1)), Value::Double(1.299_999_999_999_999_8));
        assert_eq!(
            cont(&whole(&[9, 0, 8, 7, 6, 5, 4, 3, 2, 1]), decimal(99, 2)),
            Value::Double(8.91)
        );
        let decimals = [decimal(-700, 2), decimal(333, 2)];
        assert_eq!(
            cont(&decimals, Value::Decimal { unscaled: 123, width: 3, scale: 3 }),
            decimal(-572, 2)
        );
    }

    #[test]
    fn mad_is_the_median_distance_and_mode_breaks_ties_by_arrival() {
        let values: Vec<Value> = [125, -250, 333].iter().map(|&at| decimal(at, 2)).collect();
        let dec = LogicalType::Decimal { width: 5, scale: 2 };
        assert_eq!(
            finish(Holistic::Deviation, &held(&values), None, &dec).unwrap(),
            decimal(208, 2)
        );
        let seen: Vec<Value> = [2, 1, 1, 2, 3].into_iter().map(Value::Integer).collect();
        assert_eq!(
            finish(Holistic::Mode, &held(&seen), None, &LogicalType::Integer).unwrap(),
            Value::Integer(2)
        );
        let text: Vec<Value> =
            ["a", "b", "c", "d"].iter().map(|s| Value::Varchar((*s).into())).collect();
        assert_eq!(
            finish(Holistic::Median, &held(&text), None, &LogicalType::Varchar).unwrap(),
            Value::Varchar("b".into())
        );
    }

    #[test]
    fn selection_over_numbers_answers_what_a_sort_over_values_does() {
        let mut state = 7_u64;
        let mut next = move || {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (state >> 33) % 50
        };
        let fractions = [decimal(1, 1), decimal(-25, 2), Value::Double(0.37), Value::Integer(1)];
        for len in [1_usize, 2, 3, 10, 101] {
            let wholes: Vec<Value> = (0..len).map(|_| Value::BigInt(next() as i64)).collect();
            let times: Vec<Value> = (0..len).map(|_| Value::Timestamp(next() as i64 * 7)).collect();
            let decimals: Vec<Value> = (0..len).map(|_| decimal(next() as i128 - 20, 2)).collect();
            let mut reals: Vec<Value> =
                (0..len).map(|_| Value::Double(next() as f64 / 3.0)).collect();
            reals[0] = Value::Double(f64::NAN);
            let dec = LogicalType::Decimal { width: 5, scale: 2 };
            let cases = [
                (&wholes, LogicalType::BigInt, false),
                (&times, LogicalType::Timestamp, true),
                (&decimals, dec, true),
                (&reals, LogicalType::Double, true),
            ];
            for (values, ty, continuous) in cases {
                let typed = held(values);
                assert!(!matches!(typed, Held::Values(_)), "{ty} is held as numbers");
                let both = |measure, fraction: Option<&Value>| {
                    let quick = finish(measure, &typed, fraction, &ty).unwrap();
                    let slow = finish_values(measure, values, fraction, &ty).unwrap();
                    assert_eq!(format!("{quick:?}"), format!("{slow:?}"), "{measure:?} of {ty}");
                };
                for fraction in &fractions {
                    both(Holistic::Discrete, Some(fraction));
                    if continuous {
                        both(Holistic::Continuous, Some(fraction));
                    }
                }
                both(Holistic::Median, None);
                both(Holistic::Mode, None);
                if continuous {
                    both(Holistic::Deviation, None);
                }
            }
        }
    }
}
