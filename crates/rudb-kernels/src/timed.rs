//! `avg` over a time, a timestamp or an interval, which answers in the type it read.
//!
//! The pin keeps two states for these. A time or a timestamp is a count of microseconds, so the
//! state is that count summed into a hugeint, and the answer is the quotient rounded up when the
//! remainder is more than half the count. The division truncates, so a negative sum is never
//! rounded, and that is why the average of two timestamps a microsecond apart before 1970 lands on
//! the later one. A zoned time is moved to UTC first and answers with no offset.
//!
//! An interval keeps its three fields apart through the sum and divides each one, pushing what is
//! left of the months down into days at thirty a month and what is left of the days into
//! microseconds. The last step adds the microseconds left over without dividing them, which is
//! tamnd/duckdb#18, and is kept here so the answers are the pin's.

use rudb_common::{Error, LogicalType, Result, Value};

use crate::aggregate::export::{counted, member, packed, shape, whole};
use crate::datetime::{MICROS_PER_DAY, combine};

/// The days a month counts as when the months left over are pushed down into days.
const DAYS_PER_MONTH: i64 = 30;

/// The running state of a temporal `avg`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Timed {
    /// A time or a timestamp, the microseconds summed and the rows counted, answered as `returns`.
    Micros { total: i128, count: u64, returns: LogicalType },
    /// An interval, the three fields summed apart, which is always a [`Value::Interval`].
    Span { total: Value, count: i64 },
}

impl Timed {
    /// A fresh state for `avg` answering `returns`, and `None` when that is not a temporal type.
    pub(crate) fn new(returns: &LogicalType) -> Option<Self> {
        match returns {
            LogicalType::Interval => Some(Self::Span { total: zero(), count: 0 }),
            LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::Timestamp
            | LogicalType::TimestampTz => {
                Some(Self::Micros { total: 0, count: 0, returns: returns.clone() })
            }
            _ => None,
        }
    }

    /// The state's layout, the pin's for each of the two.
    pub(crate) fn layout(argument: &LogicalType) -> Option<LogicalType> {
        match argument {
            LogicalType::Interval => Some(shape(&[
                ("count", LogicalType::BigInt),
                ("value", LogicalType::Interval),
            ])),
            LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::Timestamp
            | LogicalType::TimestampTz => {
                Some(shape(&[("count", LogicalType::UBigInt), ("value", LogicalType::HugeInt)]))
            }
            _ => None,
        }
    }

    /// Folds one value in, which is never null.
    pub(crate) fn update(&mut self, value: &Value) -> Result<()> {
        match self {
            Self::Micros { total, count, .. } => {
                *total += i128::from(micros(value)?);
                *count += 1;
            }
            Self::Span { total, count } => {
                *total = combine(total, value, false)?;
                *count += 1;
            }
        }
        Ok(())
    }

    /// Folds another state of the same call in.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (self, other) {
            (Self::Micros { total, count, .. }, Self::Micros { total: more, count: seen, .. }) => {
                *total += more;
                *count += seen;
            }
            (Self::Span { total, count }, Self::Span { total: more, count: seen }) => {
                *total = combine(total, more, false)?;
                *count += seen;
            }
            (this, other) => {
                return Err(Error::internal(format!("combining {this:?} with {other:?}")));
            }
        }
        Ok(())
    }

    /// The average, or null when no row was counted.
    pub(crate) fn finish(&self) -> Result<Value> {
        match self {
            Self::Micros { count: 0, .. } | Self::Span { count: 0, .. } => Ok(Value::Null),
            Self::Micros { total, count, returns } => {
                let count = i128::from(*count);
                let rounded = total / count + i128::from(total % count > count / 2);
                let micros = i64::try_from(rounded)
                    .map_err(|_| Error::internal(format!("an average of {rounded} microseconds")))?;
                Ok(match returns {
                    LogicalType::Time => Value::Time(micros),
                    LogicalType::TimeTz => Value::TimeTz(micros),
                    LogicalType::TimestampTz => Value::TimestampTz(micros),
                    _ => Value::Timestamp(micros),
                })
            }
            Self::Span { total, count } => {
                let Value::Interval { months, days, micros } = *total else {
                    return Err(Error::internal(format!("an interval sum of {total:?}")));
                };
                Ok(spread(i64::from(months), i64::from(days), micros, *count))
            }
        }
    }

    /// The state as the pin writes it, which it does for an empty one too.
    pub(crate) fn export(&self) -> Value {
        match self {
            Self::Micros { total, count, .. } => {
                packed(vec![("count", Value::UBigInt(*count)), ("value", Value::HugeInt(*total))])
            }
            Self::Span { total, count } => {
                packed(vec![("count", Value::BigInt(*count)), ("value", total.clone())])
            }
        }
    }

    /// Puts the state `value` wrote into this fresh one.
    pub(crate) fn import(&mut self, value: &Value) -> Result<()> {
        match self {
            Self::Micros { total, count, .. } => {
                *count = counted(member(value, "count")?)?;
                *total = whole(member(value, "value")?)?;
            }
            Self::Span { total, count } => {
                *count = i64::try_from(whole(member(value, "count")?)?)
                    .map_err(|_| Error::internal("an interval average counted past BIGINT"))?;
                *total = match member(value, "value")? {
                    Value::Null => zero(),
                    held @ Value::Interval { .. } => held.clone(),
                    held => return Err(Error::internal(format!("an interval sum of {held:?}"))),
                };
            }
        }
        Ok(())
    }
}

/// The interval of nothing, which is where a sum starts.
fn zero() -> Value {
    Value::Interval { months: 0, days: 0, micros: 0 }
}

/// The microseconds a time or a timestamp stands for, a zoned time taken to UTC inside one day.
fn micros(value: &Value) -> Result<i64> {
    match *value {
        Value::Time(micros) | Value::Timestamp(micros) | Value::TimestampTz(micros) => Ok(micros),
        Value::TimeTz(micros) => Ok(micros.rem_euclid(MICROS_PER_DAY)),
        _ => Err(Error::internal(format!("avg over {value:?}"))),
    }
}

/// An interval sum divided by `count` the pin's way, the leftover of each field pushed into the
/// next one down. The fields are the pin's widths, so the months and days narrow back the way its
/// assignments to them do.
#[expect(clippy::cast_possible_truncation, reason = "the pin narrows these the same way")]
fn spread(months: i64, days: i64, micros: i64, count: i64) -> Value {
    let mut out_days = days / count;
    let mut left_days = days % count;
    let mut left_micros = micros % count;
    let left_months = (months % count) * DAYS_PER_MONTH;
    out_days += left_months / count;
    left_days += left_months % count;
    left_micros += left_days.wrapping_mul(MICROS_PER_DAY) / count;
    Value::Interval {
        months: (months / count) as i32,
        days: out_days as i32,
        micros: micros / count + left_micros,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn average(returns: &LogicalType, values: &[Value]) -> String {
        let mut state = Timed::new(returns).expect("a temporal type");
        for value in values {
            state.update(value).expect("in range");
        }
        state.finish().expect("an answer").to_string()
    }

    fn span(months: i32, days: i32, micros: i64) -> Value {
        Value::Interval { months, days, micros }
    }

    #[test]
    fn a_timestamp_average_rounds_up_past_half_and_truncates_below_zero() {
        let stamps = |values: &[i64]| {
            let values: Vec<Value> = values.iter().map(|&v| Value::Timestamp(v)).collect();
            average(&LogicalType::Timestamp, &values)
        };
        assert_eq!(stamps(&[1, 0]), "1970-01-01 00:00:00");
        assert_eq!(stamps(&[3, 2, 0]), "1970-01-01 00:00:00.000002");
        assert_eq!(stamps(&[-5, 0, 0]), "1969-12-31 23:59:59.999999");
        assert_eq!(stamps(&[-4, 0, 0]), "1969-12-31 23:59:59.999999");
    }

    #[test]
    fn a_zoned_time_is_averaged_in_utc_inside_one_day() {
        let hour = 3_600_000_000;
        let values = [Value::TimeTz(-hour), Value::TimeTz(3 * hour)];
        assert_eq!(average(&LogicalType::TimeTz, &values), "13:00:00+00");
    }

    #[test]
    fn an_interval_average_pushes_each_remainder_down_the_way_the_pin_does() {
        let day = MICROS_PER_DAY;
        let hours = |count: i64| count * 3_600_000_000;
        let values = [span(0, 1, 0), span(0, 0, hours(2)), span(1, 0, 0)];
        assert_eq!(average(&LogicalType::Interval, &values), "10 days 08:40:00");
        assert_eq!(average(&LogicalType::Interval, &[span(1, 0, 0), span(0, 0, 0)]), "15 days");
        assert_eq!(average(&LogicalType::Interval, &[span(0, 1, 0), span(0, 0, 0)]), "12:00:00");
        assert_eq!(average(&LogicalType::Interval, &[span(0, 0, day), span(0, 0, 0)]), "12:00:00");
        // tamnd/duckdb#18: the microseconds left over are added without being divided.
        let values = [span(0, 0, 5), span(0, 0, 0), span(0, 0, 0)];
        assert_eq!(average(&LogicalType::Interval, &values), "00:00:00.000003");
        assert_eq!(average(&LogicalType::Interval, &[span(0, -3, 0), span(0, 0, 0)]), "-1 day -12:00:00");
    }

    #[test]
    fn an_empty_state_exports_zero_and_finishes_as_null() {
        let state = Timed::new(&LogicalType::Interval).expect("temporal");
        assert_eq!(state.finish().expect("an answer"), Value::Null);
        let mut read = Timed::new(&LogicalType::Interval).expect("temporal");
        read.import(&state.export()).expect("its own layout");
        assert_eq!(read, state);
    }
}
