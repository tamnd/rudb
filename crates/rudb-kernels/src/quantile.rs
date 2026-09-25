//! The aggregates that need every value before they can answer: `quantile_cont`, `quantile_disc`,
//! `median`, `mad` and `mode`.
//!
//! The values of a group are held as they arrive, nulls left out, and sorted once when the answer
//! is asked for. Every rule below is the pin's, read off its answers rather than assumed.
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

use crate::compare::order;
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
        clippy::cast_possible_wrap,
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
                    continuous(&sorted, fraction)
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
        Holistic::Median if interpolates(returns) => continuous(&sorted, Fraction::HALF),
        Holistic::Median => Ok(sorted[Fraction::HALF.discrete(sorted.len())].clone()),
        Holistic::Deviation => {
            let middle = continuous(&sorted, Fraction::HALF)?;
            let distances = sorted
                .iter()
                .map(|value| distance(value, &middle))
                .collect::<Result<Vec<Value>>>()?;
            continuous(&self::sorted(&distances)?, Fraction::HALF)
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

fn continuous(sorted: &[Value], fraction: Fraction) -> Result<Value> {
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

    fn decimal(unscaled: i128, scale: u8) -> Value {
        Value::Decimal { unscaled, width: 3, scale }
    }

    #[test]
    fn a_discrete_quantile_over_a_decimal_fraction_counts_in_whole_numbers() {
        let values: Vec<Value> = (0..10).map(Value::BigInt).collect();
        let disc = |fraction: Value| {
            finish(Holistic::Discrete, &values, Some(&fraction), &LogicalType::BigInt).unwrap()
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
            finish(Holistic::Continuous, values, Some(&fraction), &LogicalType::Double).unwrap()
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
        assert_eq!(finish(Holistic::Deviation, &values, None, &dec).unwrap(), decimal(208, 2));
        let seen: Vec<Value> = [2, 1, 1, 2, 3].into_iter().map(Value::Integer).collect();
        assert_eq!(
            finish(Holistic::Mode, &seen, None, &LogicalType::Integer).unwrap(),
            Value::Integer(2)
        );
        let text: Vec<Value> =
            ["a", "b", "c", "d"].iter().map(|s| Value::Varchar((*s).into())).collect();
        assert_eq!(
            finish(Holistic::Median, &text, None, &LogicalType::Varchar).unwrap(),
            Value::Varchar("b".into())
        );
    }
}
