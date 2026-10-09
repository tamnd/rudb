//! `extract`: one field of a date, a time, a timestamp or an interval as a `numeric`, as
//! `extract_date` and `time_part_common` in `date.c` and `timestamp_part_common` and the functions
//! next to it in `timestamp.c` compute it.
//!
//! The unit is lowercased and cut to the length of a name, and then looked up as a unit of an
//! interval and then as a reserved word of the date input. A word that is neither is not
//! recognized for the type, and a unit that the type does not have is not supported for it. An
//! infinite value gives a null for a unit that repeats, such as the month, and an infinity with
//! the sign of the value for a unit that only grows, such as the year.
//!
//! A field that is a count is an integer. The seconds and the epoch have six digits after the
//! point and the milliseconds three, and the Julian day of a timestamp is the day and the fraction
//! of it that the time is, at the scale of the division of `numeric`.

use rudb_common::SqlState;

use super::decode::part_unit;
use super::format::{date2isoweek, date2isoyear, j2day};
use super::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, Fields, Interval, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE,
    UNIX_TO_POSTGRES_USECS, USECS_PER_DAY, USECS_PER_SEC, date2j, j2date, out_of_range, split_time,
};
use crate::error::TypeError;
use crate::numeric::Numeric;
use crate::scalar::NAME_MAX_BYTES;

/// A unit of `extract`, by the `DTK_` value that the token tables give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    Julian,
    IsoYear,
    Dow,
    IsoDow,
    Doy,
    Tz,
    TzHour,
    TzMinute,
    Epoch,
    /// A reserved word of the date input other than `epoch`, such as `now`.
    Reserved,
}

const DATE: &str = "date";
const TIME: &str = "time without time zone";
const TIMETZ: &str = "time with time zone";
const TIMESTAMP: &str = "timestamp without time zone";
const TIMESTAMPTZ: &str = "timestamp with time zone";
const INTERVAL: &str = "interval";

const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
const USECS_PER_HOUR: i64 = 60 * USECS_PER_MINUTE;

/// `extract(text, date)`. `None` is a null.
pub fn extract_date(units: &str, date: i32) -> Result<Option<Numeric>, TypeError> {
    let (low, unit) = lower_units(units);
    let Some(unit) = unit else { return Err(not_recognized(&low, DATE)) };
    if date == DATE_INFINITY || date == DATE_NEGATIVE_INFINITY {
        return match unit {
            Unit::Day
            | Unit::Month
            | Unit::Quarter
            | Unit::Week
            | Unit::Dow
            | Unit::IsoDow
            | Unit::Doy => Ok(None),
            Unit::Year
            | Unit::Decade
            | Unit::Century
            | Unit::Millennium
            | Unit::Julian
            | Unit::IsoYear
            | Unit::Epoch => Ok(Some(infinity(date == DATE_NEGATIVE_INFINITY))),
            _ => Err(not_supported(&low, DATE)),
        };
    }
    let julian = i64::from(date) + i64::from(POSTGRES_EPOCH_JDATE);
    let value = match unit {
        Unit::Julian => julian,
        Unit::Epoch => (julian - i64::from(UNIX_EPOCH_JDATE)) * 86_400,
        unit => {
            let (year, month, day) = j2date(julian as i32);
            date_field(unit, year, month as i32, day as i32)
                .ok_or_else(|| not_supported(&low, DATE))?
        }
    };
    Ok(Some(Numeric::from_integer(value.into())))
}

/// `extract(text, time)`, for microseconds since midnight.
pub fn extract_time(units: &str, time: i64) -> Result<Numeric, TypeError> {
    let (low, unit) = lower_units(units);
    match unit {
        Some(Unit::Epoch) => Ok(scaled(time.into(), 6)),
        None | Some(Unit::Reserved) => Err(not_recognized(&low, TIME)),
        Some(unit) => clock_field(unit, time).ok_or_else(|| not_supported(&low, TIME)),
    }
}

/// `extract(text, timetz)`, for microseconds since midnight on the clock of the value and the
/// offset of the value in seconds west of UTC.
pub fn extract_timetz(units: &str, time: i64, zone: i32) -> Result<Numeric, TypeError> {
    let (low, unit) = lower_units(units);
    match unit {
        Some(Unit::Epoch) => {
            Ok(scaled(i128::from(time) + i128::from(zone) * i128::from(USECS_PER_SEC), 6))
        }
        None | Some(Unit::Reserved) => Err(not_recognized(&low, TIMETZ)),
        Some(unit) => zone_field(unit, zone)
            .map(|value| Numeric::from_integer(value.into()))
            .or_else(|| clock_field(unit, time))
            .ok_or_else(|| not_supported(&low, TIMETZ)),
    }
}

/// `extract(text, timestamp)`, for microseconds since 2000-01-01. `None` is a null.
pub fn extract_timestamp(units: &str, ts: i64) -> Result<Option<Numeric>, TypeError> {
    let (low, unit) = lower_units(units);
    if ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEGATIVE_INFINITY {
        return non_finite_stamp(unit, &low, ts == TIMESTAMP_NEGATIVE_INFINITY, TIMESTAMP);
    }
    let unit = unit.ok_or_else(|| not_recognized(&low, TIMESTAMP))?;
    if unit == Unit::Epoch {
        return Ok(Some(epoch(ts)));
    }
    if unit == Unit::Reserved {
        return Err(not_supported(&low, TIMESTAMP));
    }
    let fields = Fields::of_timestamp(ts).ok_or_else(|| out_of_range("timestamp"))?;
    stamp_field(unit, &fields, None).map(Some).ok_or_else(|| not_supported(&low, TIMESTAMP))
}

/// `extract(text, timestamptz)`, for microseconds since 2000-01-01 UTC, with the fields of the
/// wall clock in `zone`. `None` is a null.
pub fn extract_timestamptz(
    units: &str,
    ts: i64,
    zone: &(impl TimeZone + ?Sized),
) -> Result<Option<Numeric>, TypeError> {
    let (low, unit) = lower_units(units);
    if ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEGATIVE_INFINITY {
        return non_finite_stamp(unit, &low, ts == TIMESTAMP_NEGATIVE_INFINITY, TIMESTAMPTZ);
    }
    let unit = unit.ok_or_else(|| not_recognized(&low, TIMESTAMPTZ))?;
    if unit == Unit::Epoch {
        return Ok(Some(epoch(ts)));
    }
    if unit == Unit::Reserved {
        return Err(not_supported(&low, TIMESTAMPTZ));
    }
    // `timestamp2tm` with a zone: the UTC fields check the range, the date and the clock are those
    // of the wall clock in the zone, and the fraction of a second is the one of the instant.
    let utc = Fields::of_timestamp(ts).ok_or_else(|| out_of_range("timestamp"))?;
    let unix = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
    let offset = zone.offset_at(unix);
    let local = unix + i64::from(offset);
    let (year, month, day) =
        j2date((local.div_euclid(86_400) + i64::from(UNIX_EPOCH_JDATE)) as i32);
    let (hour, minute, second, _) = split_time(local.rem_euclid(86_400) * USECS_PER_SEC);
    let fields = Fields { year, month, day, hour, minute, second, usec: utc.usec };
    stamp_field(unit, &fields, Some(-offset))
        .map(Some)
        .ok_or_else(|| not_supported(&low, TIMESTAMPTZ))
}

/// `extract(text, interval)`. `None` is a null.
pub fn extract_interval(units: &str, interval: &Interval) -> Result<Option<Numeric>, TypeError> {
    let (low, unit) = lower_units(units);
    let unit = unit.ok_or_else(|| not_recognized(&low, INTERVAL))?;
    if *interval == Interval::INFINITY || *interval == Interval::NEGATIVE_INFINITY {
        // `NonFiniteIntervalPart`.
        return match unit {
            Unit::Microsecond
            | Unit::Millisecond
            | Unit::Second
            | Unit::Minute
            | Unit::Week
            | Unit::Month
            | Unit::Quarter => Ok(None),
            Unit::Hour
            | Unit::Day
            | Unit::Year
            | Unit::Decade
            | Unit::Century
            | Unit::Millennium
            | Unit::Epoch => Ok(Some(infinity(*interval == Interval::NEGATIVE_INFINITY))),
            _ => Err(not_supported(&low, INTERVAL)),
        };
    }
    let Interval { time, day, month } = *interval;
    if unit == Unit::Reserved {
        return Err(not_recognized(&low, INTERVAL));
    }
    if unit == Unit::Epoch {
        // A year is 365.25 days and a month 30, so four times the seconds of the days and the
        // months is a whole number.
        let quarters =
            1461 * i64::from(month / 12) + 120 * i64::from(month % 12) + 4 * i64::from(day);
        let seconds = i128::from(quarters) * i128::from(86_400 / 4);
        return Ok(Some(scaled(seconds * i128::from(USECS_PER_SEC) + i128::from(time), 6)));
    }
    // `interval2itm`.
    let (year, mon) = (i64::from(month / 12), i64::from(month % 12));
    let value = match unit {
        Unit::Day => i64::from(day),
        Unit::Week => i64::from(day / 7),
        Unit::Month => mon,
        // A field of a negative interval is the negative of the field of the interval with the
        // other sign, so the quarter is taken from the months and not from the month of the year.
        Unit::Quarter if month >= 0 => mon / 3 + 1,
        Unit::Quarter => -((-i64::from(month) % 12) / 3 + 1),
        Unit::Year => year,
        Unit::Decade => year / 10,
        Unit::Century => year / 100,
        Unit::Millennium => year / 1000,
        unit => {
            return clock_field(unit, time).map(Some).ok_or_else(|| not_supported(&low, INTERVAL));
        }
    };
    Ok(Some(Numeric::from_integer(value.into())))
}

/// `downcase_truncate_identifier`: the unit with its ASCII letters lowercased, cut to the length
/// of a name, and the unit it names.
fn lower_units(units: &str) -> (String, Option<Unit>) {
    let mut low = units.to_ascii_lowercase();
    if low.len() > NAME_MAX_BYTES {
        let mut end = NAME_MAX_BYTES;
        while !low.is_char_boundary(end) {
            end -= 1;
        }
        low.truncate(end);
    }
    let unit = part_unit(low.as_bytes());
    (low, unit)
}

/// `NonFiniteTimestampTzPart`, for a timestamp that is an infinity.
fn non_finite_stamp(
    unit: Option<Unit>,
    low: &str,
    negative: bool,
    ty: &str,
) -> Result<Option<Numeric>, TypeError> {
    match unit.ok_or_else(|| not_recognized(low, ty))? {
        Unit::Microsecond
        | Unit::Millisecond
        | Unit::Second
        | Unit::Minute
        | Unit::Hour
        | Unit::Day
        | Unit::Month
        | Unit::Quarter
        | Unit::Week
        | Unit::Dow
        | Unit::IsoDow
        | Unit::Doy
        | Unit::Tz
        | Unit::TzMinute
        | Unit::TzHour => Ok(None),
        Unit::Year
        | Unit::Decade
        | Unit::Century
        | Unit::Millennium
        | Unit::Julian
        | Unit::IsoYear
        | Unit::Epoch => Ok(Some(infinity(negative))),
        Unit::Reserved => Err(not_supported(low, ty)),
    }
}

/// A field of the fields of a timestamp, with the zone in seconds west of UTC for a
/// `timestamptz`. `None` for a unit that the type does not have.
fn stamp_field(unit: Unit, fields: &Fields, zone: Option<i32>) -> Option<Numeric> {
    let (year, month, day) = (fields.year, fields.month as i32, fields.day as i32);
    let value = match unit {
        Unit::Tz | Unit::TzHour | Unit::TzMinute => zone_field(unit, zone?)?,
        Unit::Julian => {
            let clock = (i64::from(fields.hour) * 60 + i64::from(fields.minute)) * 60
                + i64::from(fields.second);
            let micros = clock * USECS_PER_SEC + i64::from(fields.usec);
            let fraction = Numeric::from_integer(micros.into())
                .div(&Numeric::from_integer(USECS_PER_DAY.into()))
                .ok()?;
            return Numeric::from_integer(date2j(year, month, day).into()).add(&fraction).ok();
        }
        Unit::Microsecond | Unit::Millisecond | Unit::Second | Unit::Minute | Unit::Hour => {
            let time = (i64::from(fields.hour) * 60 + i64::from(fields.minute)) * USECS_PER_MINUTE
                + i64::from(fields.second) * USECS_PER_SEC
                + i64::from(fields.usec);
            return clock_field(unit, time);
        }
        unit => date_field(unit, year, month, day)?,
    };
    Some(Numeric::from_integer(value.into()))
}

/// A field of a date that a date and the timestamps have. `None` for another unit.
fn date_field(unit: Unit, year: i32, month: i32, day: i32) -> Option<i64> {
    let year64 = i64::from(year);
    Some(match unit {
        Unit::Day => i64::from(day),
        Unit::Month => i64::from(month),
        Unit::Quarter => i64::from((month - 1) / 3 + 1),
        Unit::Week => i64::from(date2isoweek(year, month, day)),
        // There is no year 0: the year before 1 is 1 BC, which is -1.
        Unit::Year if year > 0 => year64,
        Unit::Year => year64 - 1,
        // The decade 199 is 1990 to 1999, the decade 0 starts with 1 BC, and the decade -1 is
        // 11 BC to 2 BC.
        Unit::Decade if year >= 0 => year64 / 10,
        Unit::Decade => -((8 - (year64 - 1)) / 10),
        Unit::Century if year > 0 => (year64 + 99) / 100,
        Unit::Century => -((99 - (year64 - 1)) / 100),
        Unit::Millennium if year > 0 => (year64 + 999) / 1000,
        Unit::Millennium => -((999 - (year64 - 1)) / 1000),
        Unit::IsoYear => match i64::from(date2isoyear(year, month, day)) {
            iso if iso <= 0 => iso - 1,
            iso => iso,
        },
        Unit::Dow | Unit::IsoDow => match j2day(date2j(year, month, day)) {
            0 if unit == Unit::IsoDow => 7,
            dow => i64::from(dow),
        },
        Unit::Doy => i64::from(date2j(year, month, day) - date2j(year, 1, 1) + 1),
        _ => return None,
    })
}

/// A field of a clock in microseconds, which for an interval can pass 24 hours or be negative.
/// `None` for another unit.
fn clock_field(unit: Unit, time: i64) -> Option<Numeric> {
    let hour = time / USECS_PER_HOUR;
    let time = time - hour * USECS_PER_HOUR;
    let minute = time / USECS_PER_MINUTE;
    let micros = time - minute * USECS_PER_MINUTE;
    Some(match unit {
        Unit::Microsecond => Numeric::from_integer(micros.into()),
        Unit::Millisecond => scaled(micros.into(), 3),
        Unit::Second => scaled(micros.into(), 6),
        Unit::Minute => Numeric::from_integer(minute.into()),
        Unit::Hour => Numeric::from_integer(hour.into()),
        _ => return None,
    })
}

/// A field of the zone of a value, in seconds west of UTC. `None` for another unit.
fn zone_field(unit: Unit, zone: i32) -> Option<i64> {
    let east = -i64::from(zone);
    Some(match unit {
        Unit::Tz => east,
        Unit::TzMinute => (east / 60) % 60,
        Unit::TzHour => east / 3600,
        _ => return None,
    })
}

/// The seconds since 1970-01-01 of a timestamp in microseconds since 2000-01-01.
fn epoch(ts: i64) -> Numeric {
    scaled(i128::from(ts) - i128::from(UNIX_TO_POSTGRES_USECS), 6)
}

/// `int64_div_fast_to_numeric`: `value / 10^scale`, with `scale` digits after the point.
fn scaled(value: i128, scale: u32) -> Numeric {
    Numeric::from_decimal(value, scale)
}

fn infinity(negative: bool) -> Numeric {
    if negative { Numeric::NEGATIVE_INFINITY } else { Numeric::INFINITY }
}

fn not_supported(low: &str, ty: &str) -> TypeError {
    TypeError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("unit \"{low}\" not supported for type {ty}"),
    )
}

fn not_recognized(low: &str, ty: &str) -> TypeError {
    TypeError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unit \"{low}\" not recognized for type {ty}"),
    )
}
