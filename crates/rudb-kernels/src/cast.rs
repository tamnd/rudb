//! Turning a value of one type into a value of another.
//!
//! Every cast in a query is one the binder put there. Nothing here is reached because a user wrote
//! `CAST`, or rather that is only the smallest part of it: `WHERE counter > 5` casts a literal,
//! `a + b` over an `INTEGER` and a `BIGINT` casts the left side, and a `UNION` casts whichever side
//! is narrower. So this is on the path of almost every query, and the thing it has to be is exact
//! rather than fast, because the tier 0 interpreter is what tiers 1 and 2 are checked against.
//!
//! A failed cast raises, and a failed `TRY_CAST` produces null. That distinction is carried by the
//! error code rather than by a second set of functions: a conversion or a range failure is what
//! `TRY_CAST` swallows, and a cast between two types nobody has written the code for is not,
//! because turning "I have not implemented this" into a column of nulls is how a missing feature
//! becomes a wrong answer.

use rudb_common::{Error, ErrorCode, LogicalType, Result, Value, days_from_civil};
use rudb_vector::{Form, Vector};

use crate::number::{approximate, digits, fit, integral, pow10, rescale};

/// Casts every value of a vector.
///
/// A cast to the type the vector already has is free. A constant vector costs one conversion
/// rather than one per row, which matters because a literal in a predicate is a constant vector
/// and the binder casts it on the way in.
///
/// # Errors
///
/// If a value cannot be represented in the target type and `try_cast` is false, or if the pair of
/// types is one this does not handle yet.
pub fn cast(input: &Vector, target: &LogicalType, try_cast: bool) -> Result<Vector> {
    if input.logical_type() == target {
        return Ok(input.clone());
    }
    if input.is_empty() {
        return Ok(Vector::constant(target.clone(), Value::Null, 0));
    }
    if input.form() == Form::Constant {
        let single = cast_value(&input.value_at(0), target, try_cast)?;
        return Ok(Vector::constant(target.clone(), single, input.len()));
    }
    let mut values = Vec::with_capacity(input.len());
    for index in 0..input.len() {
        values.push(cast_value(&input.value_at(index), target, try_cast)?);
    }
    Vector::from_values(target.clone(), &values)
}

/// Casts one value.
///
/// Null casts to null of the target type, which is not a special case so much as the only sensible
/// reading: there is no value to convert and no conversion can fail.
///
/// # Errors
///
/// If the value cannot be represented in the target type and `try_cast` is false, or if the pair
/// of types is one this does not handle yet.
pub fn cast_value(value: &Value, target: &LogicalType, try_cast: bool) -> Result<Value> {
    if value.is_null() || matches!(target, LogicalType::Null) {
        return Ok(Value::Null);
    }
    if &value.logical_type() == target {
        return Ok(value.clone());
    }
    match convert(value, target) {
        Ok(converted) => Ok(converted),
        Err(error) if try_cast && recoverable(&error) => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

/// Whether `TRY_CAST` turns this failure into a null.
fn recoverable(error: &Error) -> bool {
    matches!(error.code(), ErrorCode::Conversion | ErrorCode::OutOfRange)
}

fn convert(value: &Value, target: &LogicalType) -> Result<Value> {
    match target {
        LogicalType::Boolean => to_boolean(value),
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt
        | LogicalType::UHugeInt => to_integer(value, target),
        LogicalType::Float => to_float(value),
        LogicalType::Double => to_double(value),
        LogicalType::Decimal { width, scale } => to_decimal(value, *width, *scale),
        LogicalType::Varchar => Ok(Value::Varchar(value.to_string())),
        LogicalType::Date => to_date(value),
        LogicalType::Timestamp => to_timestamp(value),
        other => {
            Err(Error::not_implemented(format!("a cast from {} to {other}", value.logical_type())))
        }
    }
}

/// The failure DuckDB reports for a value that does not fit, in the words DuckDB uses.
fn out_of_range(value: &Value, target: &LogicalType) -> Error {
    Error::conversion(format!(
        "Type {} with value {value} can't be cast because the value is out of range for the destination type {target}",
        value.logical_type()
    ))
}

fn not_convertible(value: &Value, target: &LogicalType) -> Error {
    Error::conversion(format!("Could not convert {} '{value}' to {target}", value.logical_type()))
}

fn to_boolean(value: &Value) -> Result<Value> {
    if let Value::Varchar(text) = value {
        return match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Ok(Value::Boolean(true)),
            "false" | "f" | "no" | "n" | "0" => Ok(Value::Boolean(false)),
            _ => Err(not_convertible(value, &LogicalType::Boolean)),
        };
    }
    match integral(value) {
        Some(whole) => Ok(Value::Boolean(whole != 0)),
        None => match approximate(value) {
            Some(number) => Ok(Value::Boolean(number != 0.0)),
            None => Err(not_convertible(value, &LogicalType::Boolean)),
        },
    }
}

fn to_integer(value: &Value, target: &LogicalType) -> Result<Value> {
    let whole = match value {
        Value::Varchar(text) => {
            parse_integer(text).ok_or_else(|| not_convertible(value, target))?
        }
        _ => match integral(value) {
            Some(whole) => whole,
            None => rounded(value, target)?,
        },
    };
    narrow(whole, value, target)
}

/// A float or a decimal as a whole number, rounded half away from zero the way DuckDB rounds.
fn rounded(value: &Value, target: &LogicalType) -> Result<i128> {
    if let Value::Decimal { unscaled, scale, .. } = *value {
        let factor = pow10(scale);
        let half = factor / 2;
        let shifted = if unscaled >= 0 { unscaled + half } else { unscaled - half };
        return Ok(shifted / factor);
    }
    let number = approximate(value).ok_or_else(|| not_convertible(value, target))?;
    if !number.is_finite() {
        return Err(out_of_range(value, target));
    }
    let number = number.round();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the range check below is what decides whether the value fits"
    )]
    if (-1.7014118346046923e38..=1.7014118346046923e38).contains(&number) {
        Ok(number as i128)
    } else {
        Err(out_of_range(value, target))
    }
}

/// A whole number as the target integer type, or the range failure.
fn narrow(whole: i128, value: &Value, target: &LogicalType) -> Result<Value> {
    fit(whole, target).ok_or_else(|| out_of_range(value, target))
}

fn parse_integer(text: &str) -> Option<i128> {
    text.trim().parse::<i128>().ok()
}

fn to_float(value: &Value) -> Result<Value> {
    let number = match value {
        Value::Varchar(text) => {
            text.trim().parse::<f64>().map_err(|_| not_convertible(value, &LogicalType::Float))?
        }
        _ => approximate(value).ok_or_else(|| not_convertible(value, &LogicalType::Float))?,
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "narrowing to a float is what a cast to FLOAT is"
    )]
    let narrowed = number as f32;
    if narrowed.is_infinite() && number.is_finite() {
        return Err(out_of_range(value, &LogicalType::Float));
    }
    Ok(Value::Float(narrowed))
}

fn to_double(value: &Value) -> Result<Value> {
    let number = match value {
        Value::Varchar(text) => {
            text.trim().parse::<f64>().map_err(|_| not_convertible(value, &LogicalType::Double))?
        }
        _ => approximate(value).ok_or_else(|| not_convertible(value, &LogicalType::Double))?,
    };
    Ok(Value::Double(number))
}

fn to_decimal(value: &Value, width: u8, scale: u8) -> Result<Value> {
    let target = LogicalType::Decimal { width, scale };
    let unscaled = match value {
        Value::Decimal { unscaled, scale: from, .. } => rescale(*unscaled, *from, scale),
        Value::Varchar(text) => {
            Some(parse_decimal(text, scale).ok_or_else(|| not_convertible(value, &target))?)
        }
        _ => match integral(value) {
            Some(whole) => whole.checked_mul(pow10(scale)),
            None => {
                let number = approximate(value).ok_or_else(|| not_convertible(value, &target))?;
                if !number.is_finite() {
                    return Err(out_of_range(value, &target));
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the width check below is what decides whether the value fits"
                )]
                let scaled = (number * pow10(scale) as f64).round() as i128;
                Some(scaled)
            }
        },
    };
    let unscaled = unscaled.ok_or_else(|| out_of_range(value, &target))?;
    if digits(unscaled) > width {
        return Err(out_of_range(value, &target));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A written decimal at the given scale, with digits past the scale rounded away.
fn parse_decimal(text: &str, scale: u8) -> Option<i128> {
    let text = text.trim();
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => (-1i128, rest),
        None => (1i128, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let written: i128 = format!("{whole}{fraction}").parse().ok()?;
    let scaled = rescale(written, u8::try_from(fraction.len()).ok()?, scale)?;
    Some(sign * scaled)
}

const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;

fn to_date(value: &Value) -> Result<Value> {
    match value {
        Value::Timestamp(micros) => i32::try_from(micros.div_euclid(MICROS_PER_DAY))
            .map(Value::Date)
            .map_err(|_| out_of_range(value, &LogicalType::Date)),
        Value::Varchar(text) => match parse_date(text.trim()) {
            Some(days) => Ok(Value::Date(days)),
            None => Err(not_convertible(value, &LogicalType::Date)),
        },
        _ => Err(not_convertible(value, &LogicalType::Date)),
    }
}

fn to_timestamp(value: &Value) -> Result<Value> {
    match value {
        Value::Date(days) => Ok(Value::Timestamp(i64::from(*days) * MICROS_PER_DAY)),
        Value::Varchar(text) => match parse_timestamp(text.trim()) {
            Some(micros) => Ok(Value::Timestamp(micros)),
            None => Err(not_convertible(value, &LogicalType::Timestamp)),
        },
        _ => Err(not_convertible(value, &LogicalType::Timestamp)),
    }
}

/// `YYYY-MM-DD` as days since the epoch.
fn parse_date(text: &str) -> Option<i32> {
    let mut parts = text.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// `YYYY-MM-DD` with an optional `HH:MM:SS[.ffffff]` after it, as microseconds since the epoch.
fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let days = i64::from(parse_date(date)?);
    let micros = match time {
        None => 0,
        Some(time) => parse_time(time)?,
    };
    Some(days * MICROS_PER_DAY + micros)
}

/// `HH:MM:SS[.ffffff]` as microseconds since midnight.
fn parse_time(text: &str) -> Option<i64> {
    let (clock, fraction) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds: i64 = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() || !(0..24).contains(&hours) {
        return None;
    }
    if !(0..60).contains(&minutes) || !(0..60).contains(&seconds) {
        return None;
    }
    let micros = match fraction {
        None => 0,
        Some(digits) => {
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let padded = format!("{digits:0<6}");
            padded.get(..6)?.parse::<i64>().ok()?
        }
    };
    Some(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast_to(value: Value, target: &LogicalType) -> Result<Value> {
        cast_value(&value, target, false)
    }

    #[test]
    fn null_casts_to_null_and_never_fails() {
        let cast = cast_to(Value::Null, &LogicalType::Integer).expect("null casts");
        assert_eq!(cast, Value::Null);
    }

    #[test]
    fn a_number_that_fits_widens_and_one_that_does_not_says_so() {
        assert_eq!(
            cast_to(Value::Integer(7), &LogicalType::BigInt).expect("7 fits"),
            Value::BigInt(7)
        );
        let error = cast_to(Value::BigInt(40_000), &LogicalType::SmallInt)
            .expect_err("40000 is not a smallint");
        assert!(error.message().contains("out of range"), "{error}");
        assert_eq!(error.code(), ErrorCode::Conversion);
    }

    /// The whole reason the error code is checked rather than a second set of functions written.
    #[test]
    fn a_try_cast_that_does_not_fit_is_null_and_one_that_is_unimplemented_still_raises() {
        let fitted = cast_value(&Value::BigInt(40_000), &LogicalType::SmallInt, true)
            .expect("try_cast swallows the range failure");
        assert_eq!(fitted, Value::Null);
        let error = cast_value(&Value::Integer(1), &LogicalType::Interval, true)
            .expect_err("try_cast does not invent an interval");
        assert_eq!(error.code(), ErrorCode::NotImplemented);
    }

    #[test]
    fn a_float_casts_to_an_integer_by_rounding_rather_than_by_truncating() {
        assert_eq!(
            cast_to(Value::Double(1.5), &LogicalType::Integer).expect("rounds"),
            Value::Integer(2)
        );
        assert_eq!(
            cast_to(Value::Double(-1.5), &LogicalType::Integer).expect("rounds away from zero"),
            Value::Integer(-2)
        );
    }

    #[test]
    fn a_string_that_is_a_number_casts_and_one_that_is_not_does_not() {
        assert_eq!(
            cast_to(Value::Varchar(" 42 ".into()), &LogicalType::Integer).expect("42"),
            Value::Integer(42)
        );
        let error = cast_to(Value::Varchar("nope".into()), &LogicalType::Integer)
            .expect_err("nope is not a number");
        assert!(error.message().contains("Could not convert"), "{error}");
    }

    #[test]
    fn anything_prints_itself_when_it_casts_to_a_string() {
        assert_eq!(
            cast_to(Value::Boolean(true), &LogicalType::Varchar).expect("prints"),
            Value::Varchar("true".into())
        );
        assert_eq!(
            cast_to(Value::Date(0), &LogicalType::Varchar).expect("prints"),
            Value::Varchar("1970-01-01".into())
        );
    }

    #[test]
    fn a_decimal_keeps_its_value_across_a_change_of_scale() {
        let target = LogicalType::decimal(10, 2).expect("a legal decimal");
        let widened =
            cast_to(Value::Decimal { unscaled: 5, width: 4, scale: 1 }, &target).expect("rescales");
        assert_eq!(widened, Value::Decimal { unscaled: 50, width: 10, scale: 2 });
        let written =
            cast_to(Value::Varchar("3.14159".into()), &target).expect("rounds to two places");
        assert_eq!(written, Value::Decimal { unscaled: 314, width: 10, scale: 2 });
    }

    #[test]
    fn a_decimal_that_needs_more_digits_than_its_width_is_caught() {
        let target = LogicalType::decimal(3, 2).expect("a legal decimal");
        let error = cast_to(Value::Integer(100), &target).expect_err("100.00 needs five digits");
        assert!(error.message().contains("out of range"), "{error}");
    }

    #[test]
    fn a_written_date_and_a_written_timestamp_read_back() {
        assert_eq!(
            cast_to(Value::Varchar("2013-07-15".into()), &LogicalType::Date).expect("a date"),
            Value::Date(days_from_civil(2013, 7, 15))
        );
        let stamp =
            cast_to(Value::Varchar("2013-07-15 10:30:00.5".into()), &LogicalType::Timestamp)
                .expect("a timestamp");
        let expected = i64::from(days_from_civil(2013, 7, 15)) * MICROS_PER_DAY
            + 10 * 3_600_000_000
            + 30 * 60_000_000
            + 500_000;
        assert_eq!(stamp, Value::Timestamp(expected));
    }

    #[test]
    fn a_date_that_is_not_a_date_is_refused_rather_than_guessed_at() {
        for text in ["2013-13-01", "2013-07", "yesterday", "2013-07-15-01"] {
            let error = cast_to(Value::Varchar(text.into()), &LogicalType::Date)
                .expect_err("this is not a date");
            assert!(error.message().contains("Could not convert"), "{text}: {error}");
        }
    }

    #[test]
    fn a_constant_vector_costs_one_conversion() {
        let input = Vector::constant(LogicalType::Integer, Value::Integer(3), 1024);
        let cast = cast(&input, &LogicalType::BigInt, false).expect("widens");
        assert_eq!(cast.form(), Form::Constant);
        assert_eq!(cast.len(), 1024);
        assert_eq!(cast.value_at(1000), Value::BigInt(3));
    }

    #[test]
    fn a_cast_to_the_type_it_already_is_is_the_same_vector() {
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("three integers");
        let cast = cast(&input, &LogicalType::Integer, false).expect("free");
        assert_eq!(cast, input);
    }

    #[test]
    fn a_null_in_a_vector_stays_null_across_a_cast() {
        let input = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("three integers");
        let cast = cast(&input, &LogicalType::Varchar, false).expect("prints");
        assert_eq!(cast.value_at(0), Value::Varchar("1".into()));
        assert_eq!(cast.value_at(1), Value::Null);
    }
}
