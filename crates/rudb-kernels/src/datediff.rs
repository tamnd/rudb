//! `date_diff` and `date_sub`, the two ways of counting a part between two moments.
//!
//! These follow upstream's `date_diff.cpp` and `date_sub.cpp`. `date_diff` counts the boundaries of
//! the part that are crossed, so a year from 2019-12-31 to 2020-01-01 is one. `date_sub` counts the
//! whole parts that fit, so the same two days are no year at all, and its months are the months of
//! `age` after a correction for the end of a month.
//!
//! A date is counted as a date by `date_diff` and as the timestamp at its midnight by `date_sub`, and
//! the two report a date too large for that in different words because they fail in different
//! places upstream. A time of day has no calendar, so only the parts shorter than a day count on it.
//! Either infinity has no count, which is a null.

use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};
use rudb_vector::{Data, Form, Vector};

use crate::datetime::{
    self, MICROS_PER_DAY, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SECOND, Part,
    days_in_month, infinite_day, infinite_stamp, iso_year_of,
};
use crate::scalar::{finish, over_valid};
use crate::shape::nulls_of;

const MICROS_PER_MILLI: i64 = 1_000;
const MICROS_PER_WEEK: i64 = 7 * MICROS_PER_DAY;

/// What is being counted, after the spellings that count the same thing have been folded together.
#[derive(Clone, Copy)]
enum Unit {
    Year,
    IsoYear,
    Month,
    Quarter,
    Decade,
    Century,
    Millennium,
    Day,
    Week,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
}

impl Unit {
    /// The unit a specifier counts in, with upstream's words for the ones it does not count.
    fn of(subtracting: bool, spelling: &str) -> Result<Self> {
        let lower = spelling.to_ascii_lowercase();
        let named = if matches!(lower.as_str(), "timezone" | "timezone_hour" | "timezone_minute") {
            None
        } else {
            Some(Part::parse(spelling)?)
        };
        Ok(match named {
            Some(Part::Year) => Self::Year,
            // Whole ISO years are whole years, so only the crossing count tells them apart.
            Some(Part::IsoYear) if subtracting => Self::Year,
            Some(Part::IsoYear) => Self::IsoYear,
            Some(Part::Month) => Self::Month,
            Some(Part::Quarter) => Self::Quarter,
            Some(Part::Decade) => Self::Decade,
            Some(Part::Century) => Self::Century,
            Some(Part::Millennium) => Self::Millennium,
            Some(
                Part::Day | Part::DayOfWeek | Part::IsoDayOfWeek | Part::DayOfYear | Part::Julian,
            ) => Self::Day,
            Some(Part::Week | Part::YearWeek) => Self::Week,
            Some(Part::Hour) => Self::Hour,
            Some(Part::Minute) => Self::Minute,
            Some(Part::Second | Part::Epoch) => Self::Second,
            Some(Part::Millisecond) => Self::Millisecond,
            Some(Part::Microsecond) => Self::Microsecond,
            Some(Part::Era | Part::Timezone | Part::TimezoneHour | Part::TimezoneMinute) | None => {
                let function = if subtracting { "DATESUB" } else { "DATEDIFF" };
                return Err(Error::not_implemented(format!(
                    "Specifier type not implemented for {function}"
                )));
            }
        })
    }

    /// The word upstream uses when this is asked of a time of day, or nothing for the units a time
    /// has.
    fn calendar_word(self) -> Option<&'static str> {
        Some(match self {
            Self::Year => "year",
            Self::IsoYear => "isoyear",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Decade => "decade",
            Self::Century => "century",
            Self::Millennium => "millennium",
            Self::Day => "day",
            Self::Week => "week",
            _ => return None,
        })
    }

    /// The microseconds in one of this unit, for the units that are a fixed length.
    fn length(self) -> i64 {
        match self {
            Self::Week => MICROS_PER_WEEK,
            Self::Day => MICROS_PER_DAY,
            Self::Hour => MICROS_PER_HOUR,
            Self::Minute => MICROS_PER_MINUTE,
            Self::Second => MICROS_PER_SECOND,
            Self::Millisecond => MICROS_PER_MILLI,
            _ => 1,
        }
    }
}

/// Which of the three kinds of moment the two arguments are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Date,
    Timestamp,
    Time,
}

impl Kind {
    fn of(ty: &LogicalType) -> Option<Self> {
        match ty {
            LogicalType::Date => Some(Self::Date),
            LogicalType::Timestamp => Some(Self::Timestamp),
            LogicalType::Time => Some(Self::Time),
            _ => None,
        }
    }

    fn infinite(self, raw: i64) -> bool {
        match self {
            Self::Date => i32::try_from(raw).is_ok_and(infinite_day),
            Self::Timestamp => infinite_stamp(raw),
            Self::Time => false,
        }
    }
}

fn subtracted(end: i64, start: i64) -> Result<i64> {
    end.checked_sub(start).ok_or_else(|| {
        Error::out_of_range(format!("Overflow in subtraction of INT64 ({end} - {start})!"))
    })
}

fn day_of(raw: i64) -> i32 {
    i32::try_from(raw).unwrap_or(i32::MAX)
}

fn day_of_stamp(stamp: i64) -> i32 {
    i32::try_from(stamp.div_euclid(MICROS_PER_DAY)).unwrap_or(i32::MAX)
}

/// A date in microseconds, as `date_diff` reads it.
fn date_micros(day: i32) -> Result<i64> {
    i64::from(day).checked_mul(MICROS_PER_DAY).ok_or_else(|| {
        Error::conversion(format!("Could not convert DATE ({}) to microseconds", Value::Date(day)))
    })
}

/// A date as the timestamp at its midnight, as `date_sub` reads it.
fn midnight(day: i32) -> Result<i64> {
    i64::from(day)
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| Error::conversion("Date and time not in timestamp range"))
}

/// Months since year zero, counted from zero, which is what the month and quarter crossings count.
fn months_of(day: i32) -> i64 {
    let (year, month, _) = civil_from_days(day);
    i64::from(year) * 12 + i64::from(month) - 1
}

fn year_of(day: i32) -> i64 {
    i64::from(civil_from_days(day).0)
}

/// Upstream's rounding towards minus infinity, which it writes out by hand.
fn floored(value: i64, units: i64) -> i64 {
    value.div_euclid(units)
}

/// The boundaries of the unit crossed between two days.
fn crossed_days(unit: Unit, start: i32, end: i32) -> Result<i64> {
    Ok(match unit {
        Unit::Year => year_of(end) - year_of(start),
        Unit::IsoYear => i64::from(iso_year_of(end)) - i64::from(iso_year_of(start)),
        Unit::Month => months_of(end) - months_of(start),
        Unit::Quarter => months_of(end) / 3 - months_of(start) / 3,
        Unit::Decade => year_of(end) / 10 - year_of(start) / 10,
        Unit::Century => year_of(end) / 100 - year_of(start) / 100,
        Unit::Millennium => year_of(end) / 1000 - year_of(start) / 1000,
        Unit::Day => i64::from(end) - i64::from(start),
        Unit::Week => (i64::from(end) - i64::from(start)) / 7,
        Unit::Microsecond => subtracted(date_micros(end)?, date_micros(start)?)?,
        Unit::Millisecond => {
            date_micros(end)? / MICROS_PER_MILLI - date_micros(start)? / MICROS_PER_MILLI
        }
        // Seconds since 1970 of a date always fit, so these three cannot fail.
        Unit::Second | Unit::Hour | Unit::Minute => {
            let seconds = unit.length() / MICROS_PER_SECOND;
            (i64::from(end) * 86_400) / seconds - (i64::from(start) * 86_400) / seconds
        }
    })
}

/// The boundaries crossed between two timestamps, where the calendar units read the dates.
fn crossed_stamps(unit: Unit, start: i64, end: i64) -> Result<i64> {
    match unit {
        Unit::Microsecond => subtracted(end, start),
        Unit::Millisecond | Unit::Second | Unit::Minute | Unit::Hour => {
            Ok(floored(end, unit.length()) - floored(start, unit.length()))
        }
        _ => crossed_days(unit, day_of_stamp(start), day_of_stamp(end)),
    }
}

/// Upstream's month count for `date_sub`, where a moment on the last day of a shorter month counts
/// as having reached the same day of a longer one.
fn whole_months(start: i64, end: i64) -> Result<i64> {
    if start > end {
        return Ok(-whole_months(end, start)?);
    }
    let mut start = start;
    let (end_day, end_time) = (day_of_stamp(end), end.rem_euclid(MICROS_PER_DAY));
    let (year, month, day) = civil_from_days(end_day);
    let end_days = days_in_month(year, month);
    if end_days == day {
        let (start_day, start_time) = (day_of_stamp(start), start.rem_euclid(MICROS_PER_DAY));
        let (year, month, day) = civil_from_days(start_day);
        if day > end_days || (day == end_days && start_time < end_time) {
            let moved = days_from_civil(year, month, end_days);
            start = i64::from(moved) * MICROS_PER_DAY + start_time;
        }
    }
    match datetime::age(&Value::Timestamp(end), &Value::Timestamp(start))? {
        Value::Interval { months, .. } => Ok(i64::from(months)),
        _ => Ok(0),
    }
}

/// The whole units that fit between two timestamps.
fn fitted_stamps(unit: Unit, start: i64, end: i64) -> Result<i64> {
    Ok(match unit {
        Unit::Year | Unit::IsoYear => whole_months(start, end)? / 12,
        Unit::Month => whole_months(start, end)?,
        Unit::Quarter => whole_months(start, end)? / 3,
        Unit::Decade => whole_months(start, end)? / 120,
        Unit::Century => whole_months(start, end)? / 1_200,
        Unit::Millennium => whole_months(start, end)? / 12_000,
        _ => subtracted(end, start)? / unit.length(),
    })
}

/// The count between two moments of one kind, each as the number the kind is stored as.
fn between(subtracting: bool, unit: Unit, kind: Kind, start: i64, end: i64) -> Result<i64> {
    match kind {
        Kind::Time => {
            if let Some(word) = unit.calendar_word() {
                return Err(Error::not_implemented(format!(
                    "\"time\" units \"{word}\" not recognized"
                )));
            }
            let length = unit.length();
            Ok(if subtracting { (end - start) / length } else { end / length - start / length })
        }
        Kind::Timestamp if subtracting => fitted_stamps(unit, start, end),
        Kind::Timestamp => crossed_stamps(unit, start, end),
        Kind::Date if subtracting => {
            fitted_stamps(unit, midnight(day_of(start))?, midnight(day_of(end))?)
        }
        Kind::Date => crossed_days(unit, day_of(start), day_of(end)),
    }
}

fn raw(value: &Value) -> Option<(Kind, i64)> {
    match value {
        Value::Date(day) => Some((Kind::Date, i64::from(*day))),
        Value::Timestamp(stamp) => Some((Kind::Timestamp, *stamp)),
        Value::Time(micros) => Some((Kind::Time, *micros)),
        _ => None,
    }
}

/// `date_diff` or `date_sub` on one row.
///
/// # Errors
///
/// A specifier that names no part, upstream's not implemented errors for the parts it does not
/// count and for a calendar part of a time, and its overflow and conversion errors.
pub(crate) fn value(name: &str, part: &Value, start: &Value, end: &Value) -> Result<Value> {
    if part.is_null() || start.is_null() || end.is_null() {
        return Ok(Value::Null);
    }
    let Value::Varchar(spelling) = part else {
        return Err(Error::internal(format!("{name} takes the part as text")));
    };
    let (Some((kind, start)), Some((other, end))) = (raw(start), raw(end)) else {
        return Err(Error::internal(format!("{name} takes two moments")));
    };
    if kind != other {
        return Err(Error::internal(format!("{name} takes two moments of one type")));
    }
    let subtracting = subtracts(name);
    let unit = Unit::of(subtracting, spelling)?;
    if kind.infinite(start) || kind.infinite(end) {
        return Ok(Value::Null);
    }
    between(subtracting, unit, kind, start, end).map(Value::BigInt)
}

fn subtracts(name: &str) -> bool {
    matches!(name, "date_sub" | "datesub")
}

/// One of the two moments, either a run of them or the one value a constant holds.
enum Side<'a> {
    Days(&'a [i32]),
    Stamps(&'a [i64]),
    One(i64),
}

impl Side<'_> {
    fn at(&self, index: usize) -> i64 {
        match self {
            Self::Days(days) => i64::from(days[index]),
            Self::Stamps(stamps) => stamps[index],
            Self::One(raw) => *raw,
        }
    }
}

fn side(vector: &Vector) -> Option<Side<'_>> {
    match vector.form() {
        Form::Flat => match vector.data()? {
            Data::Int32(days) => Some(Side::Days(days)),
            Data::Int64(stamps) => Some(Side::Stamps(stamps)),
            _ => None,
        },
        Form::Constant => match vector.constant_value()? {
            Value::Null => Some(Side::One(0)),
            value => raw(value).map(|(_, raw)| Side::One(raw)),
        },
        _ => None,
    }
}

/// `date_diff` or `date_sub` over a vector, with the part read once.
///
/// Anything other than a constant part over flat or constant moments goes the row at a time way.
pub(crate) fn vectorized(
    name: &str,
    args: &[&Vector],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let [part, start, end] = args else {
        return Ok(None);
    };
    if *returns != LogicalType::BigInt || start.logical_type() != end.logical_type() {
        return Ok(None);
    }
    let Some(kind) = Kind::of(start.logical_type()) else {
        return Ok(None);
    };
    let spelling = match part.constant_value() {
        Some(Value::Varchar(spelling)) => spelling,
        Some(Value::Null) => {
            return Ok(Some(Vector::constant(LogicalType::BigInt, Value::Null, rows)));
        }
        _ => return Ok(None),
    };
    let (Some(first), Some(second)) = (side(start), side(end)) else {
        return Ok(None);
    };
    let subtracting = subtracts(name);
    let unit = Unit::of(subtracting, spelling)?;
    let base = nulls_of(start).and(&nulls_of(end), rows);
    let mut out = vec![0i64; rows];
    let mut infinite = Vec::new();
    let validity = over_valid(rows, base, |index| {
        let (from, to) = (first.at(index), second.at(index));
        if kind.infinite(from) || kind.infinite(to) {
            infinite.push(index);
            return Ok(());
        }
        out[index] = between(subtracting, unit, kind, from, to)?;
        Ok(())
    })?;
    let validity =
        infinite.into_iter().fold(validity, |validity, index| validity.with_null(index, rows));
    finish(returns, Data::Int64(out.into()), validity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> Value {
        Value::Date(days_from_civil(year, month, day))
    }

    fn counted(name: &str, part: &str, start: &Value, end: &Value) -> String {
        match value(name, &Value::Varchar(part.into()), start, end) {
            Ok(value) => value.to_string(),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn date_diff_counts_the_boundaries_crossed() {
        let (start, end) = (day(2019, 12, 31), day(2020, 1, 1));
        assert_eq!(counted("date_diff", "year", &start, &end), "1");
        assert_eq!(counted("date_diff", "decade", &start, &end), "1");
        assert_eq!(counted("date_sub", "year", &start, &end), "0");
        assert_eq!(counted("date_diff", "week", &day(2020, 1, 14), &day(2020, 1, 1)), "-1");
        assert_eq!(counted("date_diff", "century", &day(-150, 1, 1), &day(150, 1, 1)), "2");
    }

    #[test]
    fn date_sub_counts_a_short_month_to_its_last_day() {
        let sub = |start: Value, end: Value| counted("date_sub", "month", &start, &end);
        assert_eq!(sub(day(2020, 1, 31), day(2020, 2, 29)), "1");
        assert_eq!(sub(day(2020, 1, 31), day(2020, 2, 28)), "0");
        assert_eq!(sub(day(2020, 3, 31), day(2020, 1, 31)), "-2");
        assert_eq!(sub(day(2021, 2, 28), day(2020, 2, 29)), "-12");
    }

    #[test]
    fn a_part_that_is_not_counted_is_refused_in_the_pins_words() {
        let (start, end) = (day(2020, 1, 1), day(2020, 1, 2));
        assert_eq!(
            counted("date_diff", "era", &start, &end),
            "Not implemented Error: Specifier type not implemented for DATEDIFF"
        );
        assert_eq!(
            counted("datesub", "timezone", &start, &end),
            "Not implemented Error: Specifier type not implemented for DATESUB"
        );
        let time = Value::Time(0);
        assert_eq!(
            counted("date_sub", "isoyear", &time, &time),
            "Not implemented Error: \"time\" units \"year\" not recognized"
        );
        assert_eq!(
            counted("date_diff", "julian", &time, &time),
            "Not implemented Error: \"time\" units \"day\" not recognized"
        );
    }
}
