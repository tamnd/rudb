//! `make_time`, `make_timestamp` and `make_timestamp_ns`, which build a moment out of numbers.
//!
//! These follow upstream's `make_date.cpp`. The seconds come in as a double and are split into whole
//! seconds and rounded microseconds, and the whole seconds are truncated when they are between 0 and
//! 60 and rounded half to even with a range check otherwise, which is the difference between a lossy
//! cast and a checked one upstream. A time may be 24:00:00 exactly and may have a 60th second, both
//! of which carry into the next minute or day rather than being refused.

use rudb_common::{Error, Result, Value};

use crate::cast::narrow;
use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SECOND};

/// The whole seconds and the microseconds past them, the way upstream splits a double.
fn split_seconds(seconds: f64) -> Result<(i32, i32)> {
    // A lossy cast of NaN is the smallest integer on the machines upstream runs on.
    if seconds.is_nan() {
        return Ok((i32::MIN, i32::MIN));
    }
    #[expect(clippy::cast_possible_truncation, reason = "the range is checked or known")]
    let whole = if (0.0..=60.0).contains(&seconds) {
        seconds as i32
    } else {
        let rounded = seconds.round_ties_even();
        if !(-2_147_483_648.0..2_147_483_648.0).contains(&rounded) {
            return Err(Error::invalid_input(format!(
                "Type DOUBLE with value {} can't be cast because the value is out of range for the destination type INT32",
                Value::Double(seconds)
            )));
        }
        rounded as i32
    };
    #[expect(clippy::cast_possible_truncation, reason = "upstream's cast is lossy here too")]
    let micros = ((seconds - f64::from(whole)) * 1_000_000.0).round() as i32;
    Ok((whole, micros))
}

/// Upstream's `Time::IsValidTime`.
fn valid(hour: i32, minute: i32, second: i32, micros: i32) -> bool {
    if !(0..24).contains(&hour) {
        return hour == 24 && minute == 0 && second == 0 && micros == 0;
    }
    (0..60).contains(&minute) && (0..=60).contains(&second) && (0..=1_000_000).contains(&micros)
}

/// A time of day in microseconds out of an hour, a minute and a double of seconds.
fn time_of(hour: &Value, minute: &Value, seconds: &Value) -> Result<i64> {
    let (Some(hour), Some(minute), Value::Double(seconds)) =
        (hour.as_i64(), minute.as_i64(), seconds)
    else {
        return Err(Error::internal("make_time takes two whole numbers and a double"));
    };
    let (hour, minute) = (narrow(hour)?, narrow(minute)?);
    let (second, micros) = split_seconds(*seconds)?;
    if !valid(hour, minute, second, micros) {
        return Err(Error::conversion(format!(
            "Time out of range: {hour}:{minute}:{second}.{micros}"
        )));
    }
    Ok(((i64::from(hour) * 60 + i64::from(minute)) * 60 + i64::from(second)) * MICROS_PER_SECOND
        + i64::from(micros))
}

/// A count of microseconds or nanoseconds as a moment, which must not be one of the infinities.
fn counted(count: &Value) -> Result<i64> {
    let Some(count) = count.as_i64() else {
        return Err(Error::internal("make_timestamp takes a whole number"));
    };
    if count == i64::MAX || count == -i64::MAX {
        return Err(Error::conversion(format!("Timestamp microseconds out of range: {count}")));
    }
    Ok(count)
}

/// One of the three on one row.
///
/// # Errors
///
/// Upstream's errors for a field that does not fit an INT32, a date or a time out of range, a
/// moment past the end of the timestamps and a count that is one of the infinities.
pub(crate) fn value(name: &str, args: &[Value]) -> Result<Value> {
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    match (name, args) {
        ("make_time", [hour, minute, seconds]) => Ok(Value::Time(time_of(hour, minute, seconds)?)),
        ("make_timestamp", [count]) => Ok(Value::Timestamp(counted(count)?)),
        ("make_timestamp_ns", [count]) => Ok(Value::TimestampNs(counted(count)?)),
        ("make_timestamp", [year, month, day, hour, minute, seconds]) => {
            let fields = [year, month, day].map(|field| field.as_i64().map(narrow));
            let [Some(year), Some(month), Some(day)] = fields else {
                return Err(Error::internal("make_timestamp takes whole numbers for the date"));
            };
            let (year, month, day) = (year?, month?, day?);
            let Value::Date(days) = crate::scalar::made_civil_value(
                &Value::BigInt(i64::from(year)),
                &Value::BigInt(i64::from(month)),
                &Value::BigInt(i64::from(day)),
            )?
            else {
                return Err(Error::internal("make_date answered something that is not a date"));
            };
            let time = time_of(hour, minute, seconds)?;
            i64::from(days)
                .checked_mul(MICROS_PER_DAY)
                .and_then(|midnight| midnight.checked_add(time))
                .filter(|stamp| *stamp != i64::MAX && *stamp != -i64::MAX)
                .map(Value::Timestamp)
                .ok_or_else(|| Error::conversion("Date and time not in timestamp range"))
        }
        _ => Err(Error::internal(format!("{name} was called with {} arguments", args.len()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn made(name: &str, args: &[Value]) -> String {
        match value(name, args) {
            Ok(value) => value.to_string(),
            Err(error) => error.to_string(),
        }
    }

    fn clock(hour: i64, minute: i64, seconds: f64) -> String {
        made("make_time", &[Value::BigInt(hour), Value::BigInt(minute), Value::Double(seconds)])
    }

    #[test]
    fn a_time_carries_a_sixtieth_second_and_midnight() {
        assert_eq!(clock(10, 11, 12.5), "10:11:12.5");
        assert_eq!(clock(10, 0, 59.999_999_9), "10:01:00");
        assert_eq!(clock(10, 0, 60.5), "10:01:00.5");
        assert_eq!(clock(24, 0, 0.0), "24:00:00");
    }

    #[test]
    fn a_time_out_of_range_is_refused_with_its_fields() {
        assert_eq!(clock(10, 0, -0.5), "Conversion Error: Time out of range: 10:0:0.-500000");
        assert_eq!(clock(24, 0, 0.5), "Conversion Error: Time out of range: 24:0:0.500000");
        assert_eq!(
            clock(10, 0, f64::NAN),
            "Conversion Error: Time out of range: 10:0:-2147483648.-2147483648"
        );
    }
}
