//! `in_range`, the support function that a window frame of `RANGE` with an offset calls in
//! PostgreSQL, one for each type of the btree families that have one.
//!
//! The executor asks it whether a row is at or past the place `base - offset` or `base + offset`,
//! and it answers without the arithmetic failing where the place is outside the type. An integer
//! sum that overflows is past every value, a NaN sorts after every other number, and an infinite
//! offset from an infinite base of the same side covers every value. A negative offset, and a NaN
//! offset, is the error of the function, and it comes from the first row that asks.

use std::cmp::Ordering;

use rudb_common::{Error, Result, SqlState, Value, time_tz};
use rudb_pgtypes::{Numeric, NumericSign, USECS_PER_DAY};

/// `in_range(val, base, offset, sub, less)`: with `less`, whether `val <= base - offset` when
/// `sub` and `val <= base + offset` when not; without `less`, the same with `>=`. The values have
/// the type of the column, and the offset has the type that the binder chose for it from the
/// in_range functions of that type.
///
/// # Errors
/// An offset that is negative or NaN, and a sum that the type of a date or a time cannot hold.
pub fn in_range(val: &Value, base: &Value, offset: &Value, sub: bool, less: bool) -> Result<bool> {
    match (val, base, offset) {
        (Value::Float(_) | Value::Double(_), _, _) => {
            let (Some(val), Some(base), Some(offset)) = (real(val), real(base), real(offset))
            else {
                return Err(unexpected(val, offset));
            };
            floats(val, base, offset, sub, less)
        }
        (Value::Numeric(_) | Value::Decimal { .. }, _, _) => {
            match (numeric(val), numeric(base), numeric(offset)) {
                (Some(val), Some(base), Some(offset)) => numerics(&val, &base, &offset, sub, less),
                _ => Err(unexpected(val, offset)),
            }
        }
        (Value::Date(val), Value::Date(base), Value::Interval { .. }) => {
            let val = Value::Timestamp(date_as_timestamp(*val)?);
            let base = Value::Timestamp(date_as_timestamp(*base)?);
            stamps(&val, &base, offset, sub, less)
        }
        (Value::Timestamp(_) | Value::TimestampTz(_), _, Value::Interval { .. }) => {
            stamps(val, base, offset, sub, less)
        }
        (Value::Time(val), Value::Time(base), &Value::Interval { micros, .. }) => {
            times(*val, *base, micros, sub, less)
        }
        (Value::TimeTz(val), Value::TimeTz(base), &Value::Interval { micros, .. }) => {
            zoned_times(*val, *base, micros, sub, less)
        }
        (
            &Value::Interval { months, days, micros },
            &Value::Interval { months: bm, days: bd, micros: bt },
            &Value::Interval { months: om, days: od, micros: ot },
        ) => intervals((months, days, micros), (bm, bd, bt), (om, od, ot), sub, less),
        _ => match (whole(val), whole(base), whole(offset)) {
            (Some(val), Some(base), Some(offset)) => integers(val, base, offset, sub, less),
            _ => Err(unexpected(val, offset)),
        },
    }
}

/// `ERRCODE_INVALID_PRECEDING_OR_FOLLOWING_SIZE`, which each in_range function raises for an
/// offset that is negative, or NaN, before it reads the values.
fn invalid_size() -> Error {
    Error::invalid_input("invalid preceding or following size in window function")
        .state(SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE)
        .unplaced()
}

fn unexpected(val: &Value, offset: &Value) -> Error {
    Error::internal(format!(
        "no in_range function for {} with an offset of {}",
        val.logical_type(),
        offset.logical_type()
    ))
}

/// The answer from where `val` is against `sum`.
fn against(ordering: Ordering, less: bool) -> bool {
    if less { ordering != Ordering::Greater } else { ordering != Ordering::Less }
}

fn whole(value: &Value) -> Option<i64> {
    match *value {
        Value::SmallInt(value) => Some(i64::from(value)),
        Value::Integer(value) => Some(i64::from(value)),
        Value::BigInt(value) => Some(value),
        _ => None,
    }
}

/// A `numeric`, which a column with a precision holds as a decimal.
fn numeric(value: &Value) -> Option<Numeric> {
    match *value {
        Value::Numeric(ref bytes) => Some(Numeric::from_bytes(bytes)),
        Value::Decimal { unscaled, scale, .. } => {
            Some(Numeric::from_decimal(unscaled, u32::from(scale)))
        }
        _ => None,
    }
}

fn real(value: &Value) -> Option<f64> {
    match *value {
        Value::Float(value) => Some(f64::from(value)),
        Value::Double(value) => Some(value),
        _ => None,
    }
}

/// `in_range_int4_int8` and the others of `int2`, `int4` and `int8`. A sum that overflows the
/// type is past every value of it, so the answer is the one for a sum at the end that the offset
/// runs to. The sum is taken in 128 bits, which cannot overflow and gives the same answers.
fn integers(val: i64, base: i64, offset: i64, sub: bool, less: bool) -> Result<bool> {
    if offset < 0 {
        return Err(invalid_size());
    }
    let sum = if sub {
        i128::from(base) - i128::from(offset)
    } else {
        i128::from(base) + i128::from(offset)
    };
    Ok(against(i128::from(val).cmp(&sum), less))
}

/// `in_range_float8_float8`, which `in_range_float4_float8` calls with the values made wider.
fn floats(val: f64, base: f64, offset: f64, sub: bool, less: bool) -> Result<bool> {
    if offset.is_nan() || offset < 0.0 {
        return Err(invalid_size());
    }
    // NaN sorts after every other value, as `float8_cmp_internal` has it, and the offset cannot
    // change that.
    if val.is_nan() {
        return Ok(if base.is_nan() { true } else { !less });
    }
    if base.is_nan() {
        return Ok(less);
    }
    // An infinite offset from an infinite base of the side it runs from is NaN as a sum. The frame
    // takes every value then, as +inf infinitely precedes +inf.
    if offset.is_infinite() && base.is_infinite() && (if sub { base > 0.0 } else { base < 0.0 }) {
        return Ok(true);
    }
    let sum = if sub { base - offset } else { base + offset };
    Ok(if less { val <= sum } else { val >= sum })
}

/// `in_range_numeric_numeric`.
fn numerics(
    val: &Numeric,
    base: &Numeric,
    offset: &Numeric,
    sub: bool,
    less: bool,
) -> Result<bool> {
    if matches!(
        offset.sign(),
        NumericSign::NaN | NumericSign::NegativeInfinity | NumericSign::Negative
    ) {
        return Err(invalid_size());
    }
    let nan = |value: &Numeric| value.sign() == NumericSign::NaN;
    let infinity = |value: &Numeric| value.sign() == NumericSign::Infinity;
    let negative_infinity = |value: &Numeric| value.sign() == NumericSign::NegativeInfinity;
    let special = |value: &Numeric| infinity(value) || negative_infinity(value);
    Ok(if nan(val) {
        if nan(base) { true } else { !less }
    } else if nan(base) {
        less
    } else if infinity(offset) {
        if if sub { infinity(base) } else { negative_infinity(base) } {
            true
        } else if sub {
            // The sum is -inf, and only -inf is at or below it.
            if less { negative_infinity(val) } else { true }
        } else if less {
            true
        } else {
            infinity(val)
        }
    } else if special(val) {
        if infinity(val) {
            if infinity(base) { true } else { !less }
        } else if negative_infinity(base) {
            true
        } else {
            less
        }
    } else if special(base) {
        if negative_infinity(base) { !less } else { less }
    } else {
        let sum = if sub { base.sub(offset) } else { base.add(offset) }?;
        against(val.compare(&sum), less)
    })
}

/// `date2timestamp`: midnight of a date, with the infinities kept.
fn date_as_timestamp(days: i32) -> Result<i64> {
    match days {
        i32::MAX => Ok(i64::MAX),
        days if days == -i32::MAX => Ok(-i64::MAX),
        days => i64::from(days).checked_mul(USECS_PER_DAY).ok_or_else(|| {
            Error::out_of_range("date out of range for timestamp")
                .state(SqlState::DATETIME_VALUE_OUT_OF_RANGE)
        }),
    }
}

/// The time an interval spans in microseconds, as `interval_cmp_value` counts it, with a month of
/// 30 days. The infinities are the largest and the smallest of these.
fn span((months, days, micros): (i32, i32, i64)) -> i128 {
    let days = i128::from(days) + i128::from(months) * 30;
    i128::from(micros) + days * i128::from(USECS_PER_DAY)
}

fn is_infinity((months, days, micros): (i32, i32, i64)) -> bool {
    (months, days, micros) == (i32::MAX, i32::MAX, i64::MAX)
}

fn is_negative_infinity((months, days, micros): (i32, i32, i64)) -> bool {
    (months, days, micros) == (i32::MIN, i32::MIN, i64::MIN)
}

fn offset_parts(offset: &Value) -> (i32, i32, i64) {
    match *offset {
        Value::Interval { months, days, micros } => (months, days, micros),
        _ => (0, 0, 0),
    }
}

/// `in_range_timestamp_interval` and `in_range_timestamptz_interval`, which `in_range_date_interval`
/// calls with the dates as timestamps. The arithmetic of a finite sum is the `+` and `-` of the
/// engine, which adds the months, then the days, then the time, as PostgreSQL does.
fn stamps(val: &Value, base: &Value, offset: &Value, sub: bool, less: bool) -> Result<bool> {
    let parts = offset_parts(offset);
    if span(parts) < 0 {
        return Err(invalid_size());
    }
    let micros = |value: &Value| match *value {
        Value::Timestamp(micros) | Value::TimestampTz(micros) => micros,
        _ => 0,
    };
    let (at, from) = (micros(val), micros(base));
    let (end, start) = (i64::MAX, -i64::MAX);
    if is_infinity(parts) && (if sub { from == end } else { from == start }) {
        return Ok(true);
    }
    let sum = if from == end || from == start {
        from
    } else if is_infinity(parts) {
        if sub { start } else { end }
    } else {
        let returns = base.logical_type();
        let name = if sub { "-" } else { "+" };
        let args = [base.clone(), offset.clone()];
        micros(&crate::call_values(name, &args, &returns, None)?)
    };
    Ok(against(at.cmp(&sum), less))
}

/// `in_range_time_interval`. The months and the days of the offset do not count, as they do not
/// for `time + interval`, and the sum does not come round at midnight. A sum past the end of
/// the 64 bits is past every time.
fn times(val: i64, base: i64, offset: i64, sub: bool, less: bool) -> Result<bool> {
    if offset < 0 {
        return Err(invalid_size());
    }
    let sum = if sub {
        base - offset
    } else {
        match base.checked_add(offset) {
            Some(sum) => sum,
            None => return Ok(less),
        }
    };
    Ok(against(val.cmp(&sum), less))
}

/// `in_range_timetz_interval`: as [`times`], with the zone of the base kept on the sum, and the
/// two compared by `timetz_cmp_internal`, which is the time at UTC and then the zone.
fn zoned_times(val: i64, base: i64, offset: i64, sub: bool, less: bool) -> Result<bool> {
    if offset < 0 {
        return Err(invalid_size());
    }
    let from = time_tz::micros(base);
    let sum = if sub {
        from - offset
    } else {
        match from.checked_add(offset) {
            Some(sum) => sum,
            None => return Ok(less),
        }
    };
    // PostgreSQL holds the zone in seconds west of UTC, so its key is the time minus the offset
    // east, and a larger zone is a smaller offset east.
    let key = |micros: i64, east: i32| {
        (i128::from(micros) - i128::from(east) * 1_000_000, -i64::from(east))
    };
    let zone = time_tz::offset(base);
    let ordering = key(time_tz::micros(val), time_tz::offset(val)).cmp(&key(sum, zone));
    Ok(against(ordering, less))
}

/// `in_range_interval_interval`, with the sum of `interval_pl` and `interval_mi`, which keeps an
/// infinite base and makes a finite base infinite with an infinite offset.
fn intervals(
    val: (i32, i32, i64),
    base: (i32, i32, i64),
    offset: (i32, i32, i64),
    sub: bool,
    less: bool,
) -> Result<bool> {
    if span(offset) < 0 {
        return Err(invalid_size());
    }
    if is_infinity(offset) && (if sub { is_infinity(base) } else { is_negative_infinity(base) }) {
        return Ok(true);
    }
    let sum = if is_infinity(base) || is_negative_infinity(base) {
        span(base)
    } else if is_infinity(offset) {
        span(if sub { (i32::MIN, i32::MIN, i64::MIN) } else { (i32::MAX, i32::MAX, i64::MAX) })
    } else {
        let field = |left: i64, right: i64| if sub { left - right } else { left + right };
        let months = i32::try_from(field(i64::from(base.0), i64::from(offset.0)));
        let days = i32::try_from(field(i64::from(base.1), i64::from(offset.1)));
        let micros = if sub { base.2.checked_sub(offset.2) } else { base.2.checked_add(offset.2) };
        match (months, days, micros) {
            (Ok(months), Ok(days), Some(micros))
                if !is_infinity((months, days, micros))
                    && !is_negative_infinity((months, days, micros)) =>
            {
                span((months, days, micros))
            }
            _ => {
                return Err(Error::out_of_range("interval out of range")
                    .state(SqlState::DATETIME_VALUE_OUT_OF_RANGE)
                    .unplaced());
            }
        }
    };
    Ok(against(span(val).cmp(&sum), less))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(months: i32, days: i32, micros: i64) -> Value {
        Value::Interval { months, days, micros }
    }

    #[test]
    fn an_integer_sum_past_the_type_is_past_every_value() {
        let (val, base) = (Value::Integer(i32::MAX), Value::Integer(i32::MAX - 1));
        // The end of `x RANGE BETWEEN CURRENT ROW AND 4 FOLLOWING` at the top of int4.
        assert!(in_range(&val, &base, &Value::Integer(4), false, true).unwrap());
        // 7 is past 10 - 4 with an offset of another width.
        assert!(
            !in_range(&Value::Integer(7), &Value::Integer(10), &Value::BigInt(4), true, true)
                .unwrap()
        );
        let error = in_range(&val, &base, &Value::Integer(-1), true, false).unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE));
    }

    #[test]
    fn a_float_follows_the_rules_of_nan_and_infinity() {
        let inf = Value::Double(f64::INFINITY);
        // `'inf' PRECEDING` from +inf takes every value.
        assert!(in_range(&Value::Double(1.0), &inf, &inf, true, false).unwrap());
        assert!(
            in_range(&Value::Double(f64::NAN), &Value::Double(1.0), &inf, false, false).unwrap()
        );
        assert!(
            !in_range(&Value::Double(f64::NAN), &Value::Double(1.0), &inf, false, true).unwrap()
        );
        assert!(
            in_range(&Value::Float(1.5), &Value::Float(1.0), &Value::Double(0.5), false, true)
                .unwrap()
        );
        assert!(in_range(&inf, &inf, &Value::Double(f64::NAN), true, false).is_err());
    }

    #[test]
    fn a_numeric_follows_the_rules_of_nan_and_infinity() {
        let number =
            |text: &str| Value::Numeric(rudb_pgtypes::numeric_in(text, -1).unwrap().to_bytes());
        let inf = number("Infinity");
        assert!(in_range(&number("-Infinity"), &number("1"), &inf, true, true).unwrap());
        assert!(!in_range(&number("0"), &number("1"), &inf, true, true).unwrap());
        assert!(in_range(&number("2.1"), &number("1"), &number("1.1"), false, true).unwrap());
        assert!(!in_range(&number("2.2"), &number("1"), &number("1.1"), false, true).unwrap());
        assert!(in_range(&number("1"), &number("1"), &number("NaN"), false, true).is_err());
    }

    #[test]
    fn a_time_does_not_come_round_at_midnight() {
        let hour = 3_600_000_000;
        let (val, base) = (Value::Time(hour), Value::Time(23 * hour));
        // 23:00 + 2 hours is 25:00 and not 01:00, so 01:00 is not past it.
        assert!(in_range(&val, &base, &interval(0, 0, 2 * hour), false, true).unwrap());
        assert!(!in_range(&val, &base, &interval(0, 0, 2 * hour), false, false).unwrap());
        let infinity = interval(i32::MAX, i32::MAX, i64::MAX);
        assert!(in_range(&val, &base, &infinity, false, true).unwrap());
        let error = in_range(&val, &base, &interval(i32::MIN, i32::MIN, i64::MIN), false, true);
        assert!(error.is_err());
    }

    #[test]
    fn an_infinite_interval_offset_takes_every_value_from_an_infinite_base() {
        let infinity = interval(i32::MAX, i32::MAX, i64::MAX);
        let year = interval(12, 0, 0);
        assert!(in_range(&year, &infinity, &infinity, true, false).unwrap());
        assert!(!in_range(&year, &infinity, &infinity, false, false).unwrap());
        // A year compares as 360 days, so 380 days is within a year and 30 days and past a year
        // less 30 days.
        assert!(in_range(&interval(0, 380, 0), &year, &interval(0, 30, 0), false, true).unwrap());
        assert!(!in_range(&interval(0, 380, 0), &year, &interval(0, 30, 0), true, true).unwrap());
        let stamp = Value::Timestamp(0);
        assert!(in_range(&stamp, &Value::Timestamp(i64::MAX), &infinity, true, true).unwrap());
        assert!(in_range(&stamp, &Value::Timestamp(-i64::MAX), &year, false, false).unwrap());
        let date = Value::Date(10);
        assert!(in_range(&date, &Value::Date(0), &interval(0, 10, 0), false, true).unwrap());
        assert!(!in_range(&date, &Value::Date(0), &interval(0, 9, 0), false, true).unwrap());
    }
}
