//! The date and time functions of `pg_proc`, by the name of the C function in `prosrc`, as
//! `src/backend/utils/adt/timestamp.c` writes them.
//!
//! Every function here is strict, so the caller gives a null for a null argument and the
//! functions see only values.
//!
//! `extract` over a `timestamptz` reads the wall clock in the session time zone, so it is called
//! with the zone by [`zoned_call`], and [`call`] gives it the clock at UTC.

use rudb_common::{Error, Result, SessionTimeZone, SqlState, Value, time_tz};
use rudb_pgtypes::{Interval, Numeric, TypeError, date_from_unix, timestamp_from_unix};

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "extract_date",
    "extract_interval",
    "extract_time",
    "extract_timestamp",
    "extract_timestamptz",
    "extract_timetz",
    "make_interval",
];

/// The C functions of this module that read the session time zone.
pub(crate) const ZONED: &[&str] = &["extract_timestamptz"];

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    if ZONED.contains(&src) {
        return zoned_call(src, args, SessionTimeZone::default()).map(Some);
    }
    let value = match (src, args) {
        ("extract_date", [Value::Varchar(units), Value::Date(days)]) => {
            let date = date_from_unix(*days).map_err(placed)?;
            numeric(rudb_pgtypes::extract_date(units, date))?
        }
        ("extract_time", [Value::Varchar(units), Value::Time(micros)]) => {
            numeric(rudb_pgtypes::extract_time(units, *micros).map(Some))?
        }
        ("extract_timetz", [Value::Varchar(units), Value::TimeTz(key)]) => {
            let (time, zone) = (time_tz::micros(*key), -time_tz::offset(*key));
            numeric(rudb_pgtypes::extract_timetz(units, time, zone).map(Some))?
        }
        ("extract_timestamp", [Value::Varchar(units), Value::Timestamp(micros)]) => {
            let ts = timestamp_from_unix(*micros).map_err(placed)?;
            numeric(rudb_pgtypes::extract_timestamp(units, ts))?
        }
        ("extract_interval", [Value::Varchar(units), Value::Interval { months, days, micros }]) => {
            let interval = Interval { time: *micros, day: *days, month: *months };
            numeric(rudb_pgtypes::extract_interval(units, &interval))?
        }
        (
            "make_interval",
            [
                Value::Integer(years),
                Value::Integer(months),
                Value::Integer(weeks),
                Value::Integer(days),
                Value::Integer(hours),
                Value::Integer(mins),
                Value::Double(secs),
            ],
        ) => make_interval([*years, *months, *weeks, *days, *hours, *mins], *secs)?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: f64 = 1_000_000.0;

/// `make_interval(years, months, weeks, days, hours, mins, secs)`. A part that overflows its
/// field of the interval, a second that is not finite and a result that is one of the infinite
/// intervals are `interval out of range`.
fn make_interval(parts: [i32; 6], secs: f64) -> Result<Value> {
    let [years, months, weeks, days, hours, mins] = parts;
    if !secs.is_finite() {
        return Err(out_of_range());
    }
    let months =
        years.checked_mul(12).and_then(|held| held.checked_add(months)).ok_or_else(out_of_range)?;
    let days =
        weeks.checked_mul(7).and_then(|held| held.checked_add(days)).ok_or_else(out_of_range)?;
    // The hours and the minutes cannot overflow 64 bits.
    let time = i64::from(hours) * USECS_PER_HOUR + i64::from(mins) * USECS_PER_MINUTE;
    // `float8_mul`, then `rint`.
    let micros = secs * USECS_PER_SEC;
    if micros.is_infinite() {
        return Err(Error::out_of_range("value out of range: overflow")
            .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
            .unplaced());
    }
    let micros = micros.round_ties_even();
    // `FLOAT8_FITS_IN_INT64`.
    #[expect(clippy::cast_precision_loss, reason = "the bound is a power of two")]
    let bound = -(i64::MIN as f64);
    if !(micros >= -bound && micros < bound) {
        return Err(out_of_range());
    }
    #[expect(clippy::cast_possible_truncation, reason = "the value fits, as checked above")]
    let time = time.checked_add(micros as i64).ok_or_else(out_of_range)?;
    // `INTERVAL_NOT_FINITE`.
    let infinite = (months == i32::MAX && days == i32::MAX && time == i64::MAX)
        || (months == i32::MIN && days == i32::MIN && time == i64::MIN);
    if infinite {
        return Err(out_of_range());
    }
    Ok(Value::Interval { months, days, micros: time })
}

/// The value of the C function `src` of [`ZONED`] over `args`, in the time zone `zone`.
pub(crate) fn zoned_call(src: &str, args: &[Value], zone: SessionTimeZone) -> Result<Value> {
    match (src, args) {
        ("extract_timestamptz", [Value::Varchar(units), Value::TimestampTz(micros)]) => {
            let ts = timestamp_from_unix(*micros).map_err(placed)?;
            numeric(rudb_pgtypes::extract_timestamptz(units, ts, &zone))
        }
        _ => Err(Error::internal(format!("{src} over {args:?}"))),
    }
}

/// A `numeric` answer, or a null for `None`.
fn numeric(answer: std::result::Result<Option<Numeric>, TypeError>) -> Result<Value> {
    Ok(answer.map_err(placed)?.map_or(Value::Null, |number| Value::Numeric(number.to_bytes())))
}

/// An error of PostgreSQL that has no position, as the errors of a function that runs have.
fn placed(error: TypeError) -> Error {
    Error::from(error).unplaced()
}

fn out_of_range() -> Error {
    Error::out_of_range("interval out of range")
        .state(SqlState::DATETIME_VALUE_OUT_OF_RANGE)
        .unplaced()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(args: [i32; 6], secs: f64) -> String {
        match make_interval(args, secs) {
            Ok(value) => value.to_string(),
            Err(error) => format!("{} {}", error.reported_state(), error.message()),
        }
    }

    #[test]
    fn make_interval_adds_the_parts_and_checks_each_field_as_postgresql_does() {
        assert_eq!(interval([1, 2, 3, 4, 5, 6], 7.5), interval([0, 14, 0, 25, 0, 0], 18367.5));
        assert_eq!(
            make_interval([0; 6], 1.5).unwrap(),
            Value::Interval { months: 0, days: 0, micros: 1_500_000 }
        );
        // `rint` rounds a half to the even microsecond.
        assert_eq!(
            make_interval([0; 6], 0.000_000_5).unwrap(),
            make_interval([0; 6], 0.0).unwrap()
        );
        for (args, secs, expected) in [
            ([i32::MAX, 0, 0, 0, 0, 0], 0.0, "22008 interval out of range"),
            ([0, 0, i32::MAX, 0, 0, 0], 0.0, "22008 interval out of range"),
            ([0; 6], f64::INFINITY, "22008 interval out of range"),
            ([0; 6], f64::NAN, "22008 interval out of range"),
            ([0; 6], 1e303, "22003 value out of range: overflow"),
            ([0; 6], 1e300, "22008 interval out of range"),
            ([0; 6], 1e14, "22008 interval out of range"),
        ] {
            assert_eq!(interval(args, secs), expected, "{args:?} {secs}");
        }
    }
}
