//! `time_bucket`, which rounds a date, a timestamp or a time down to the start of the bucket of a
//! given width that it falls in.
//!
//! This follows upstream's `time_bucket.cpp`. A width is either a count of microseconds, when it has
//! no months, or a count of months, when it has nothing else. Buckets of microseconds are counted
//! from 2000-01-03, a Monday, and buckets of months from 2000-01-01, which is what TimescaleDB does.
//! The third argument either moves that origin to a moment of the caller's choosing or, when it is
//! an interval, shifts the moment back by it before the rounding and forward again after.
//!
//! A time is bucketed as the timestamp it is on 1970-01-01 and comes back as the time of day of the
//! answer, so bucketing a time by months is always midnight.

use rudb_common::{Error, Result, Value, civil_from_days, days_from_civil};

use crate::datetime::{MICROS_PER_DAY, infinite_day, infinite_stamp, shifted_stamp};

/// 2000-01-03 in microseconds since 1970.
const ORIGIN_MICROS: i64 = 10_959 * MICROS_PER_DAY;
/// 2000-01-01 in months since 1970.
const ORIGIN_MONTHS: i32 = 360;

/// What a width is counted in.
enum Width {
    Micros(i64),
    Months(i32),
}

/// Upstream's `ClassifyBucketWidthErrorThrow`.
fn width(months: i32, days: i32, micros: i64) -> Result<Width> {
    if months == 0 {
        let total = i128::from(days) * i128::from(MICROS_PER_DAY) + i128::from(micros);
        let total = i64::try_from(total).unwrap_or(i64::MAX);
        if total <= 0 {
            return Err(Error::not_implemented("Period must be greater than 0"));
        }
        Ok(Width::Micros(total))
    } else if days == 0 && micros == 0 {
        if months < 0 {
            return Err(Error::not_implemented("Period must be greater than 0"));
        }
        Ok(Width::Months(months))
    } else {
        Err(Error::not_implemented("Month intervals cannot have day or time component"))
    }
}

fn added64(left: i64, right: i64) -> Result<i64> {
    left.checked_add(right).ok_or_else(|| {
        Error::out_of_range(format!("Overflow in addition of INT64 ({left} + {right})!"))
    })
}

fn subtracted64(left: i64, right: i64) -> Result<i64> {
    left.checked_sub(right).ok_or_else(|| {
        Error::out_of_range(format!("Overflow in subtraction of INT64 ({left} - {right})!"))
    })
}

fn added32(left: i32, right: i32) -> Result<i32> {
    left.checked_add(right).ok_or_else(|| {
        Error::out_of_range(format!("Overflow in addition of INT32 ({left} + {right})!"))
    })
}

fn subtracted32(left: i32, right: i32) -> Result<i32> {
    left.checked_sub(right).ok_or_else(|| {
        Error::out_of_range(format!("Overflow in subtraction of INT32 ({left} - {right})!"))
    })
}

/// Upstream's `WidthConvertibleToMicrosCommon`, which works in C's truncating division and so
/// steps a negative moment that is not on a boundary back by one more bucket.
fn micros_bucket(width: i64, stamp: i64, origin: i64) -> Result<i64> {
    let origin = origin % width;
    let stamp = subtracted64(stamp, origin)?;
    let mut bucket = (stamp / width) * width;
    if stamp < 0 && stamp % width != 0 {
        bucket = subtracted64(bucket, width)?;
    }
    added64(bucket, origin)
}

/// Upstream's `WidthConvertibleToMonthsCommon`, which answers the first day of the month.
fn months_bucket(width: i32, months: i32, origin: i32) -> Result<i32> {
    let origin = origin % width;
    let months = subtracted32(months, origin)?;
    let mut bucket = (months / width) * width;
    if months < 0 && months % width != 0 {
        bucket = subtracted32(bucket, width)?;
    }
    let bucket = added32(bucket, origin)?;
    let year = 1970 + bucket.div_euclid(12);
    let month = u32::try_from(bucket.rem_euclid(12) + 1).unwrap_or(1);
    Ok(days_from_civil(year, month, 1))
}

/// A moment's months since January 1970.
fn epoch_months(days: i32) -> i32 {
    let (year, month, _) = civil_from_days(days);
    (year - 1970) * 12 + i32::try_from(month).unwrap_or(1) - 1
}

/// A moment as the timestamp upstream buckets it as.
fn stamp_of(when: &Value) -> i64 {
    match when {
        Value::Date(day) => i64::from(*day) * MICROS_PER_DAY,
        Value::Timestamp(stamp) | Value::Time(stamp) => *stamp,
        _ => 0,
    }
}

/// A moment as the date upstream buckets it as by months, where a time is 1970-01-01.
fn day_of(when: &Value) -> i32 {
    match when {
        Value::Date(day) => *day,
        Value::Timestamp(stamp) => day_of_stamp(*stamp),
        _ => 0,
    }
}

fn day_of_stamp(stamp: i64) -> i32 {
    i32::try_from(stamp.div_euclid(MICROS_PER_DAY)).unwrap_or(i32::MAX)
}

/// A bucketed timestamp as the type the moment came in.
fn from_stamp(like: &Value, stamp: i64) -> Value {
    match like {
        Value::Date(_) => Value::Date(day_of_stamp(stamp)),
        Value::Time(_) => Value::Time(stamp.rem_euclid(MICROS_PER_DAY)),
        _ => Value::Timestamp(stamp),
    }
}

/// A bucketed date as the type the moment came in.
fn from_day(like: &Value, day: i32) -> Value {
    match like {
        Value::Date(_) => Value::Date(day),
        Value::Time(_) => Value::Time(0),
        _ => Value::Timestamp(i64::from(day) * MICROS_PER_DAY),
    }
}

fn infinite(when: &Value) -> bool {
    match when {
        Value::Date(day) => infinite_day(*day),
        Value::Timestamp(stamp) => infinite_stamp(*stamp),
        _ => false,
    }
}

/// `time_bucket` on one row, with two arguments or three.
///
/// # Errors
///
/// The pin's not implemented errors for a width that is not positive or that mixes months with
/// days or time, and its overflow errors for a bucket out of range.
pub(crate) fn value(args: &[Value]) -> Result<Value> {
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let (Value::Interval { months, days, micros }, when) = (&args[0], &args[1]) else {
        return Err(Error::internal("time_bucket takes an interval first"));
    };
    let third = args.get(2);
    // An origin at an infinity has no buckets, and that is answered before the width is looked at.
    if third.is_some_and(infinite) {
        return Ok(Value::Null);
    }
    let width = width(*months, *days, *micros)?;
    if infinite(when) {
        return Ok(when.clone());
    }
    match (width, third) {
        (Width::Micros(width), None) => {
            Ok(from_stamp(when, micros_bucket(width, stamp_of(when), ORIGIN_MICROS)?))
        }
        (Width::Months(width), None) => {
            Ok(from_day(when, months_bucket(width, epoch_months(day_of(when)), ORIGIN_MONTHS)?))
        }
        (width, Some(Value::Interval { months, days, micros })) => {
            let (months, days) = (i64::from(*months), i64::from(*days));
            let back = shifted_stamp(stamp_of(when), -months, -days, -i128::from(*micros))?;
            let bucket = match width {
                Width::Micros(width) => micros_bucket(width, back, ORIGIN_MICROS)?,
                Width::Months(width) => {
                    let day =
                        months_bucket(width, epoch_months(day_of_stamp(back)), ORIGIN_MONTHS)?;
                    i64::from(day) * MICROS_PER_DAY
                }
            };
            Ok(from_stamp(when, shifted_stamp(bucket, months, days, i128::from(*micros))?))
        }
        (Width::Micros(width), Some(origin)) => {
            Ok(from_stamp(when, micros_bucket(width, stamp_of(when), stamp_of(origin))?))
        }
        (Width::Months(width), Some(origin)) => {
            let bucket =
                months_bucket(width, epoch_months(day_of(when)), epoch_months(day_of(origin)))?;
            Ok(from_day(when, bucket))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> Value {
        Value::Date(days_from_civil(year, month, day))
    }

    fn interval(months: i32, days: i32, micros: i64) -> Value {
        Value::Interval { months, days, micros }
    }

    #[test]
    fn a_date_is_bucketed_from_the_timescale_origins() {
        let when = day(2019, 4, 5);
        assert_eq!(value(&[interval(0, 2, 0), when.clone()]).unwrap(), day(2019, 4, 5));
        assert_eq!(value(&[interval(1, 0, 0), when.clone()]).unwrap(), day(2019, 4, 1));
        assert_eq!(value(&[interval(3, 0, 0), day(1965, 2, 3)]).unwrap(), day(1965, 1, 1));
        assert_eq!(value(&[interval(0, 7, 0), day(1965, 2, 3)]).unwrap(), day(1965, 2, 1));
        assert_eq!(
            value(&[interval(0, 1, 0), when, interval(0, 0, -3 * 3_600_000_000)]).unwrap(),
            day(2019, 4, 4)
        );
    }

    #[test]
    fn a_bucket_out_of_range_is_the_pins_overflow() {
        let said = value(&[interval(i32::MAX, 0, 0), day(1700, 1, 1), day(1800, 1, 1)])
            .unwrap_err()
            .to_string();
        assert!(said.contains("Overflow in addition of INT32 (-2147483647 + -2040)!"), "{said}");
    }
}
