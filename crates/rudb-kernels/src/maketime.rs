//! `make_time`, `make_timestamp` and the rest of the calls that build a moment out of numbers.
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

/// The seconds since 1970 in a double as microseconds, which `to_timestamp` rounds half to even
/// the way the pin's checked cast to `BIGINT` does.
fn epoch_seconds(seconds: f64) -> Result<i64> {
    let micros = (seconds * 1_000_000.0).round_ties_even();
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&micros) {
        return Err(Error::conversion("Epoch seconds out of range for TIMESTAMP WITH TIME ZONE"));
    }
    #[expect(clippy::cast_possible_truncation, reason = "the range is checked above")]
    Ok(micros as i64)
}

/// The wall clock `make_timestamptz` names, in microseconds, which the pin works out with a lenient
/// ICU calendar.
///
/// Every field is checked into an INT32 and then carried, so month 13 is January of the next year
/// and day 0 is the last day of the month before. A year below zero counts from 1 BC rather than
/// from year 0, so `-1` and `0` are both 1 BC. The seconds are rounded half to even into whole
/// seconds and what is left is split into milliseconds and rounded microseconds, which is how
/// `61.5` comes out a second and a half past the minute after next.
///
/// # Errors
///
/// A field that does not fit an INT32, and a wall clock past the end of the timestamps.
pub(crate) fn wall_clock(fields: &[Value]) -> Result<i64> {
    let [year, month, day, hour, minute, Value::Double(seconds)] = fields else {
        return Err(Error::internal("make_timestamptz takes five whole numbers and a double"));
    };
    let whole = |field: &Value| {
        field.as_i64().ok_or_else(|| Error::internal("make_timestamptz takes whole numbers"))
    };
    let year = whole(year)?;
    let year = narrow(year + i64::from(year < 0))?;
    let month = narrow(whole(month)?.checked_sub(1).ok_or_else(|| {
        Error::out_of_range("Overflow in subtraction of INT64 (-9223372036854775808 - 1)!")
    })?)?;
    let (day, hour, minute) =
        (narrow(whole(day)?)?, narrow(whole(hour)?)?, narrow(whole(minute)?)?);
    let rounded = seconds.round_ties_even();
    if !(-2_147_483_648.0..2_147_483_648.0).contains(&rounded) {
        return Err(Error::invalid_input(format!(
            "Type DOUBLE with value {} can't be cast because the value is out of range for the destination type INT32",
            Value::Double(*seconds)
        )));
    }
    let left = (seconds - rounded) * 1_000.0;
    #[expect(clippy::cast_possible_truncation, reason = "the pin truncates the milliseconds")]
    let millis = left as i64;
    #[expect(clippy::cast_possible_truncation, reason = "a fraction of a millisecond")]
    let micros = ((left - millis as f64) * 1_000.0).round() as i64;
    // The year is taken a 400 year cycle at a time, since the carried one need not fit a date.
    let months = i64::from(year) * 12 + i64::from(month);
    let (cycles, year) = (months.div_euclid(12 * 400), months.rem_euclid(12 * 400));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the year is within one cycle and the month is 1 to 12"
    )]
    let first = rudb_common::days_from_civil((year / 12) as i32, (year % 12 + 1) as u32, 1);
    let days = i128::from(cycles) * 146_097 + i128::from(first) + i128::from(day) - 1;
    let seconds = (i128::from(hour) * 60 + i128::from(minute)) * 60 + rounded as i128;
    let total = (days * 86_400 + seconds) * 1_000_000 + i128::from(millis * 1_000 + micros);
    i64::try_from(total).map_err(|_| Error::conversion("ICU date overflows timestamp range"))
}

/// One of them on one row.
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
        ("make_timestamptz", [count]) => Ok(Value::TimestampTz(counted(count)?)),
        ("make_timestamp_ms", [count]) => count
            .as_i64()
            .and_then(|millis| millis.checked_mul(1_000))
            .map(Value::Timestamp)
            .ok_or_else(|| Error::conversion("Could not convert Timestamp(MS) to Timestamp(US)")),
        ("to_timestamp", [Value::Double(seconds)]) => {
            Ok(Value::TimestampTz(epoch_seconds(*seconds)?))
        }
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
