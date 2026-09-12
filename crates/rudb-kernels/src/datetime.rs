//! The calendar, which is what `date_part` and `date_trunc` are made of.
//!
//! Both functions are one string and one date or timestamp, and the string decides which of twenty
//! pieces of arithmetic happens. Splitting the string out into a [`Part`] is not tidiness: the
//! string is a literal in every query anyone writes, so the vectorized path reads it once per vector
//! and the loop underneath it does one piece of arithmetic per row with nothing left to decide.
//!
//! The arithmetic is written out rather than pulled in from a date library, for the reason
//! [`rudb_common::civil_from_days`] gives: a library that disagrees with DuckDB about a week number
//! or about a century before year one is a compatibility bug we would then own without being able to
//! fix it. Every number this file produces was read off the DuckDB binary on `server3` first, and
//! the tests below are those readings rather than a second opinion about what ISO 8601 says.
//!
//! Two of DuckDB's answers are worth knowing before reading the code, because both look like
//! mistakes and neither is one. `date_part('millisecond', ...)` carries the seconds with it, so
//! 59.654321 seconds is 59654 and not 654. And `date_part('century', ...)` of the year 2000 is 20,
//! because the twentieth century ends with it, while `date_trunc('century', ...)` of the same
//! timestamp is the year 2000 and not the year 1901, because truncation drops the last two digits.
//! DuckDB is not being consistent there and neither are we, on purpose.
//!
//! What is missing is `timezone`, `timezone_hour` and `timezone_minute`, which need a session time
//! zone before they mean anything, and the interval overloads of both functions.

use rudb_common::{Error, Result, Value, civil_from_days, days_from_civil};

/// Microseconds in a day, which is the conversion between the two representations here.
pub(crate) const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;

pub(crate) const MICROS_PER_HOUR: i64 = 3_600 * 1_000_000;
pub(crate) const MICROS_PER_MINUTE: i64 = 60 * 1_000_000;
pub(crate) const MICROS_PER_SECOND: i64 = 1_000_000;

/// A piece of a date or a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    /// The ISO 8601 week, 1 through 53.
    Week,
    Quarter,
    /// Sunday 0 through Saturday 6, which is Postgres's numbering and DuckDB's.
    DayOfWeek,
    /// Monday 1 through Sunday 7.
    IsoDayOfWeek,
    DayOfYear,
    Decade,
    Century,
    Millennium,
    /// 1 for a year after year zero and 0 for one before it.
    Era,
    /// The year the ISO week belongs to, which is not the calendar year at the turn of it.
    IsoYear,
    /// The ISO year and week as one number, so week 9 of 2024 is 202409.
    YearWeek,
    /// Seconds since 1970, which DuckDB answers as a double and this does not answer at all.
    Epoch,
}

/// Every spelling DuckDB accepts, each one checked against the binary rather than guessed at.
///
/// A linear scan rather than a map because the list is fifty entries, because it is walked once per
/// vector on the path that matters, and because a map would have to be built at startup to answer a
/// question that a scan answers in the time the allocation alone would cost.
const NAMES: &[(&str, Part)] = &[
    ("year", Part::Year),
    ("years", Part::Year),
    ("yr", Part::Year),
    ("y", Part::Year),
    ("month", Part::Month),
    ("months", Part::Month),
    ("mon", Part::Month),
    ("mons", Part::Month),
    ("day", Part::Day),
    ("days", Part::Day),
    ("d", Part::Day),
    ("hour", Part::Hour),
    ("hours", Part::Hour),
    ("hr", Part::Hour),
    ("h", Part::Hour),
    ("minute", Part::Minute),
    ("minutes", Part::Minute),
    ("min", Part::Minute),
    ("mins", Part::Minute),
    ("m", Part::Minute),
    ("second", Part::Second),
    ("seconds", Part::Second),
    ("sec", Part::Second),
    ("secs", Part::Second),
    ("s", Part::Second),
    ("millisecond", Part::Millisecond),
    ("milliseconds", Part::Millisecond),
    ("msec", Part::Millisecond),
    ("msecs", Part::Millisecond),
    ("ms", Part::Millisecond),
    ("microsecond", Part::Microsecond),
    ("microseconds", Part::Microsecond),
    ("usec", Part::Microsecond),
    ("usecs", Part::Microsecond),
    ("us", Part::Microsecond),
    ("week", Part::Week),
    ("weeks", Part::Week),
    ("w", Part::Week),
    ("quarter", Part::Quarter),
    ("quarters", Part::Quarter),
    ("dayofweek", Part::DayOfWeek),
    ("dow", Part::DayOfWeek),
    ("weekday", Part::DayOfWeek),
    ("isodow", Part::IsoDayOfWeek),
    ("dayofyear", Part::DayOfYear),
    ("doy", Part::DayOfYear),
    ("decade", Part::Decade),
    ("decades", Part::Decade),
    ("dec", Part::Decade),
    ("century", Part::Century),
    ("centuries", Part::Century),
    ("cent", Part::Century),
    ("millennium", Part::Millennium),
    ("millenniums", Part::Millennium),
    ("mil", Part::Millennium),
    ("era", Part::Era),
    ("isoyear", Part::IsoYear),
    ("yearweek", Part::YearWeek),
    ("epoch", Part::Epoch),
];

impl Part {
    /// Which part a specifier names.
    ///
    /// # Errors
    ///
    /// If it names none of them, with DuckDB's own message, since a query that asks for `qtr`
    /// should be told the same thing by both engines.
    pub(crate) fn parse(spelling: &str) -> Result<Self> {
        NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(spelling))
            .map(|(_, part)| *part)
            .ok_or_else(|| {
                Error::conversion(format!("extract specifier \"{spelling}\" not recognized"))
            })
    }

    /// The part of a date, which is a count of days since 1970-01-01.
    ///
    /// # Errors
    ///
    /// If the part is one this file does not answer.
    pub(crate) fn of_days(self, days: i32) -> Result<i64> {
        let (year, month, day) = civil_from_days(days);
        let wide = i64::from(year);
        Ok(match self {
            // A date has no time in it, and DuckDB says the time parts of one are zero rather than
            // refusing to answer about them.
            Self::Hour | Self::Minute | Self::Second | Self::Millisecond | Self::Microsecond => 0,
            Self::Year => wide,
            Self::Month => i64::from(month),
            Self::Day => i64::from(day),
            Self::Week => i64::from(iso_week(days).1),
            Self::Quarter => i64::from((month - 1) / 3 + 1),
            // Day zero is a Thursday, so the shift that puts Sunday at zero is four.
            Self::DayOfWeek => i64::from((days + 4).rem_euclid(7)),
            Self::IsoDayOfWeek => i64::from(iso_weekday(days)),
            Self::DayOfYear => i64::from(days - days_from_civil(year, 1, 1) + 1),
            Self::Decade => wide / 10,
            // The first century is the years 1 to 100, so the arithmetic is off by one on both
            // sides of year zero and there is no year zero in the counting at all.
            Self::Century => {
                if year > 0 {
                    (wide - 1) / 100 + 1
                } else {
                    wide / 100 - 1
                }
            }
            Self::Millennium => {
                if year > 0 {
                    (wide - 1) / 1_000 + 1
                } else {
                    wide / 1_000 - 1
                }
            }
            Self::Era => i64::from(year > 0),
            Self::IsoYear => i64::from(iso_week(days).0),
            Self::YearWeek => {
                let (year, week) = iso_week(days);
                i64::from(year) * 100 + i64::from(week)
            }
            // DuckDB answers this as a double, and a signature that says bigint cannot hand one
            // back. The fix is an overload rather than a cast, so this says so rather than rounding.
            Self::Epoch => {
                return Err(Error::not_implemented(
                    "date_part('epoch', ...), which DuckDB answers as a double",
                ));
            }
        })
    }

    /// The part of a timestamp, which is a count of microseconds since 1970-01-01.
    ///
    /// # Errors
    ///
    /// If the part is one this file does not answer, or if the timestamp is outside the range a
    /// date covers and the part needs the date.
    pub(crate) fn of_micros(self, micros: i64) -> Result<i64> {
        let within = micros.rem_euclid(MICROS_PER_DAY);
        Ok(match self {
            Self::Hour => within / MICROS_PER_HOUR,
            Self::Minute => within / MICROS_PER_MINUTE % 60,
            Self::Second => within / MICROS_PER_SECOND % 60,
            // These two carry the seconds with them. It looks wrong and it is what DuckDB and
            // Postgres both answer, so 59.654321 seconds is 59654 and 59654321.
            Self::Millisecond => within / 1_000 % 60_000,
            Self::Microsecond => within % 60_000_000,
            _ => return self.of_days(day_of(micros)?),
        })
    }

    /// A date truncated to the part.
    ///
    /// # Errors
    ///
    /// If the part is one nothing can be truncated to.
    pub(crate) fn truncate_days(self, days: i32) -> Result<i32> {
        let (year, month, _) = civil_from_days(days);
        Ok(match self {
            // Everything below a day leaves a date alone, and so do the three parts that name a day
            // rather than a length, which is what DuckDB answers for `date_trunc('dow', ...)`.
            Self::Microsecond
            | Self::Millisecond
            | Self::Second
            | Self::Minute
            | Self::Hour
            | Self::Day
            | Self::DayOfWeek
            | Self::IsoDayOfWeek
            | Self::DayOfYear
            | Self::Epoch => days,
            Self::Week | Self::YearWeek => days - (iso_weekday(days) - 1),
            Self::Month => days_from_civil(year, month, 1),
            Self::Quarter => days_from_civil(year, (month - 1) / 3 * 3 + 1, 1),
            Self::Year => days_from_civil(year, 1, 1),
            // Truncation drops digits rather than counting centuries, so this is the year 2000 and
            // not the year 2001 even though the century containing 2000 is the twentieth.
            Self::Decade => days_from_civil(year - year % 10, 1, 1),
            Self::Century => days_from_civil(year - year % 100, 1, 1),
            Self::Millennium => days_from_civil(year - year % 1_000, 1, 1),
            Self::IsoYear => iso_year_start(iso_week(days).0),
            // DuckDB has no truncation for an era and says so rather than guessing, and this is its
            // message down to the word statistics, which is a word about where in DuckDB the check
            // happens to live.
            Self::Era => {
                return Err(Error::not_implemented(
                    "Specifier type not implemented for DATETRUNC statistics",
                ));
            }
        })
    }

    /// A timestamp truncated to the part.
    ///
    /// # Errors
    ///
    /// If the part is one nothing can be truncated to, or if the timestamp is outside the range a
    /// date covers and the part needs the date.
    pub(crate) fn truncate_micros(self, micros: i64) -> Result<i64> {
        // A floor rather than a truncation towards zero at every step, so that a timestamp before
        // the epoch lands on the boundary below it and not the one above it.
        Ok(match self {
            Self::Microsecond => micros,
            Self::Millisecond => micros - micros.rem_euclid(1_000),
            Self::Second | Self::Epoch => micros - micros.rem_euclid(MICROS_PER_SECOND),
            Self::Minute => micros - micros.rem_euclid(MICROS_PER_MINUTE),
            Self::Hour => micros - micros.rem_euclid(MICROS_PER_HOUR),
            _ => i64::from(self.truncate_days(day_of(micros)?)?) * MICROS_PER_DAY,
        })
    }
}

/// The thirteen functions that build an interval out of a count of one unit.
///
/// These are what `INTERVAL 1 DAY` is by the time the transformer is finished with it, since
/// DuckDB rewrites the literal into an ordinary call and names the column after the call. So the
/// literal and a handwritten `to_days(1)` are the same expression and cannot drift apart.
///
/// An interval keeps months, days and microseconds in three separate fields and never converts
/// between them, which is the whole reason a month plus a day is not a number of anything. Each
/// function below lands in exactly one of the three, so a week is seven days rather than 604800
/// seconds, and `to_weeks(3)` prints `21 days` rather than `3 weeks`.
///
/// The counts that do not fit are upstream's sentence, and it names the unit that was asked for
/// rather than the field it lands in, so overflowing `to_years` says years and not months. Two of
/// the thirteen take their count as a DOUBLE and keep the fraction, `to_seconds(2.7)` being
/// `00:00:02.7`, so the multiply for those happens in floating point and the truncation happens
/// after it. That is also why their message prints six digits after the point: it is the count that
/// was passed and not the integer it ends up as.
pub(crate) fn interval(name: &str, count: Count) -> Result<(i32, i32, i64)> {
    let unit = name.strip_prefix("to_").unwrap_or(name);
    let (field, scale) = match unit {
        "years" => (Field::Months, 12),
        "months" => (Field::Months, 1),
        "quarters" => (Field::Months, 3),
        "decades" => (Field::Months, 120),
        "centuries" => (Field::Months, 1_200),
        "millennia" => (Field::Months, 12_000),
        "days" => (Field::Days, 1),
        "weeks" => (Field::Days, 7),
        "hours" => (Field::Micros, i128::from(MICROS_PER_HOUR)),
        "minutes" => (Field::Micros, i128::from(MICROS_PER_MINUTE)),
        "seconds" => (Field::Micros, i128::from(MICROS_PER_SECOND)),
        "milliseconds" => (Field::Micros, 1_000),
        "microseconds" => (Field::Micros, 1),
        _ => return Err(Error::internal(format!("{name} is not an interval constructor"))),
    };
    let written = match count {
        Count::Whole(whole) => whole.to_string(),
        Count::Real(real) => format!("{real:.6}"),
    };
    let refuse = || Error::out_of_range(format!("Interval value {written} {unit} out of range"));
    let total = match count {
        Count::Whole(whole) => whole.checked_mul(scale).ok_or_else(refuse)?,
        // A double that has gone past `i128` comes back as the saturated bound, which is out of
        // every field's range as well, so the check below catches it without a case of its own.
        Count::Real(real) => (real * scale as f64).trunc() as i128,
    };
    match field {
        Field::Months => Ok((i32::try_from(total).map_err(|_| refuse())?, 0, 0)),
        Field::Days => Ok((0, i32::try_from(total).map_err(|_| refuse())?, 0)),
        Field::Micros => Ok((0, 0, i64::try_from(total).map_err(|_| refuse())?)),
    }
}

/// A count of one unit, as the type the function that was called takes it as.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Count {
    Whole(i128),
    Real(f64),
}

/// Which of an interval's three fields a constructor lands in.
enum Field {
    Months,
    Days,
    Micros,
}

/// Whether a name is one of the interval constructors above.
pub(crate) fn is_interval(name: &str) -> bool {
    matches!(
        name,
        "to_years"
            | "to_months"
            | "to_quarters"
            | "to_decades"
            | "to_centuries"
            | "to_millennia"
            | "to_days"
            | "to_weeks"
            | "to_hours"
            | "to_minutes"
            | "to_seconds"
            | "to_milliseconds"
            | "to_microseconds"
    )
}

/// The day a timestamp falls on.
///
/// A floor and not a truncation towards zero, so that a time before the epoch lands on the day it
/// is in rather than on the day after it.
fn day_of(micros: i64) -> Result<i32> {
    i32::try_from(micros.div_euclid(MICROS_PER_DAY))
        .map_err(|_| Error::conversion(format!("timestamp {micros} is outside the date range")))
}

/// The ISO weekday, Monday 1 through Sunday 7.
fn iso_weekday(days: i32) -> i32 {
    // Day zero is a Thursday, which is ISO weekday four.
    (days + 3).rem_euclid(7) + 1
}

/// The ISO year and the ISO week a day falls in.
///
/// The rule is one sentence: a week belongs to the year its Thursday is in. Everything people find
/// hard about ISO week numbers, including the fact that 1 January 2021 is week 53 of 2020, falls out
/// of that sentence rather than needing a case of its own.
fn iso_week(days: i32) -> (i32, i32) {
    let thursday = days + (4 - iso_weekday(days));
    let (year, _, _) = civil_from_days(thursday);
    let week = (thursday - days_from_civil(year, 1, 1)) / 7 + 1;
    (year, week)
}

/// The day an ISO year starts on, which is the Monday of the week 4 January is in.
fn iso_year_start(year: i32) -> i32 {
    let fourth = days_from_civil(year, 1, 4);
    fourth - (iso_weekday(fourth) - 1)
}

/// How many days that month of that year has.
///
/// A date that names the thirty first of April is out of range upstream and was the first of May
/// here, which is a wrong answer and not only a wrong message.
pub(crate) fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

/// The oldest and the newest timestamp, which is not the whole of an `i64` of microseconds.
///
/// The top of the `i64` is `infinity` upstream, so the newest timestamp is the one below it, which
/// is `294247-01-10 04:00:54.775806`. The oldest is the first midnight that fits, which is
/// `290309-12-22 (BC) 00:00:00`, because the day before it does not fit at all and a timestamp is
/// a whole day plus a time inside it.
pub(crate) const OLDEST_TIMESTAMP: i64 = -9_223_372_022_400_000_000;

pub(crate) const NEWEST_TIMESTAMP: i64 = i64::MAX - 1;

/// The oldest and the newest date, which is the `i32` with the two infinities and one more taken
/// off it. `5877642-06-25 (BC)` and `5881580-07-10`.
const OLDEST_DATE: i32 = i32::MIN + 2;

const NEWEST_DATE: i32 = i32::MAX - 1;

/// The years the date range covers, which is the gate the month arithmetic passes before it counts
/// any days, because a year far outside this is a day count that does not fit an `i32` at all.
const YEARS: std::ops::RangeInclusive<i64> = -5_877_641..=5_881_580;

/// Whether a pair of values is date arithmetic rather than the numeric kind.
///
/// One side is an interval and the other is a date, a timestamp or a time. The signature has
/// already refused everything else that shares the spelling, so this only has to tell the two
/// apart and not police them.
pub(crate) fn is_shift(left: &Value, right: &Value) -> bool {
    let when =
        |value: &Value| matches!(value, Value::Date(_) | Value::Timestamp(_) | Value::Time(_));
    let interval = |value: &Value| matches!(value, Value::Interval { .. });
    (interval(left) && when(right)) || (when(left) && interval(right))
}

/// A date, a timestamp or a time with an interval added to it or taken off it.
///
/// The three fields are applied one at a time, months then days then microseconds, which is
/// upstream's order and is visible whenever a month lands on a day the next month does not have.
/// Adding a month to the thirty first of January is the twenty ninth of February in a leap year,
/// so the day clamps rather than spilling into March, and adding a month and a day to it is the
/// first of March rather than the second, because the clamp happens before the day is added.
///
/// A date comes back as a timestamp and not as a date, since the interval can carry a time of day,
/// and a time comes back as a time that wraps at midnight and ignores the months and the days,
/// both of which were measured rather than assumed.
pub(crate) fn shift(left: &Value, right: &Value, subtract: bool) -> Result<Value> {
    let (when, interval) = match (left, right) {
        (when, Value::Interval { months, days, micros }) => (when, (months, days, micros)),
        (Value::Interval { months, days, micros }, when) => (when, (months, days, micros)),
        _ => return Err(Error::internal(format!("{left} and {right} are not a shift"))),
    };
    let sign = if subtract { -1 } else { 1 };
    let (months, days, micros) = interval;
    let months = i64::from(*months) * sign;
    let days = i64::from(*days) * sign;
    let micros = i128::from(*micros) * i128::from(sign);
    match when {
        Value::Date(day) => {
            // The date becomes a timestamp before anything is added to it, which is upstream's
            // order and is why a date too old or too new to be a moment fails as a moment rather
            // than as a date, even at the newest date there is with one day added to it.
            moved(*day, 0, 0)?;
            Ok(Value::Timestamp(moved(shifted_days(*day, months, days)?, 0, micros)?))
        }
        Value::Timestamp(stamp) => {
            let day =
                i32::try_from(stamp.div_euclid(MICROS_PER_DAY)).map_err(|_| not_in_range())?;
            let within = stamp.rem_euclid(MICROS_PER_DAY);
            Ok(Value::Timestamp(moved(shifted_days(day, months, days)?, within, micros)?))
        }
        // A time is a clock and not a point in history, so the whole days go nowhere and what is
        // left wraps. `TIME '10:00:00' + INTERVAL '-1 day 1 hour'` is eleven in the morning.
        Value::Time(clock) => {
            let day = i128::from(MICROS_PER_DAY);
            let wrapped = (i128::from(*clock) + micros).rem_euclid(day);
            Ok(Value::Time(i64::try_from(wrapped).map_err(|_| not_in_range())?))
        }
        other => Err(Error::internal(format!("{other} takes no interval"))),
    }
}

/// The day an interval's months and days land on, which is where both of the date range failures
/// are and where upstream has a different sentence for each of them.
fn shifted_days(day: i32, months: i64, days: i64) -> Result<i32> {
    let day = if months == 0 { day } else { shifted_months(day, months)? };
    let moved = i64::from(day) + days;
    match i32::try_from(moved) {
        Ok(moved) if (OLDEST_DATE..=NEWEST_DATE).contains(&moved) => Ok(moved),
        _ => Err(Error::out_of_range("Date out of range")),
    }
}

/// The calendar add, which is the one piece of this that is not a count.
fn shifted_months(day: i32, months: i64) -> Result<i32> {
    let (year, month, day) = civil_from_days(day);
    let total = i64::from(year) * 12 + i64::from(month) - 1 + months;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    #[expect(clippy::cast_possible_truncation, reason = "a month of the year is one of twelve")]
    let month = month as u32;
    // The message names the day the clamp produced, so it is worked out before the range is
    // checked, and it is printed the way upstream prints it, which is unpadded and signed rather
    // than in the era a date prints in.
    let year_holds = YEARS.contains(&year);
    #[expect(clippy::cast_possible_truncation, reason = "the range above fits an i32")]
    let narrow = year as i32;
    let day = day.min(days_in_month(narrow, month));
    let out_of_range = || Error::conversion(format!("Date out of range: {year}-{month}-{day}"));
    if !year_holds {
        return Err(out_of_range());
    }
    let moved = days_from_civil(narrow, month, day);
    if !(OLDEST_DATE..=NEWEST_DATE).contains(&moved) {
        return Err(out_of_range());
    }
    Ok(moved)
}

/// The day, the time inside it and the interval's own microseconds, as one timestamp.
fn moved(day: i32, within: i64, micros: i128) -> Result<i64> {
    let stamp = i128::from(day) * i128::from(MICROS_PER_DAY) + i128::from(within) + micros;
    match i64::try_from(stamp) {
        Ok(stamp) if (OLDEST_TIMESTAMP..=NEWEST_TIMESTAMP).contains(&stamp) => Ok(stamp),
        _ => Err(not_in_range()),
    }
}

fn not_in_range() -> Error {
    Error::conversion("Date and time not in timestamp range")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-02-29 13:45:59.654321, which is a leap day, a Thursday, and in ISO week 9.
    fn moment() -> i64 {
        i64::from(days_from_civil(2024, 2, 29)) * MICROS_PER_DAY
            + 13 * MICROS_PER_HOUR
            + 45 * MICROS_PER_MINUTE
            + 59 * MICROS_PER_SECOND
            + 654_321
    }

    fn part(spelling: &str) -> Part {
        Part::parse(spelling).expect("a part this file knows")
    }

    fn day(year: i32, month: u32, day: u32) -> Value {
        Value::Date(days_from_civil(year, month, day))
    }

    fn stamp(year: i32, month: u32, day: u32, micros: i64) -> Value {
        Value::Timestamp(i64::from(days_from_civil(year, month, day)) * MICROS_PER_DAY + micros)
    }

    fn every(months: i32, days: i32, micros: i64) -> Value {
        Value::Interval { months, days, micros }
    }

    /// What the shift prints, which is what the statement it came from prints.
    fn shown(left: &Value, right: &Value, subtract: bool) -> String {
        shift(left, right, subtract).expect("the shift lands in range").to_string()
    }

    /// Every row here is a statement that was run against the pinned binary for #393.
    ///
    /// The clamp is the rule worth naming. A month added to the thirty first of January is the end
    /// of February and not the first or the second of March, and it clamps whichever way the month
    /// moves, which is why the third row goes backwards.
    #[test]
    fn a_date_or_a_timestamp_and_an_interval_make_a_timestamp() {
        let date = day(2020, 1, 1);
        assert_eq!(shown(&date, &every(1, 0, 0), false), "2020-02-01 00:00:00");
        assert_eq!(shown(&every(1, 0, 0), &date, false), "2020-02-01 00:00:00");
        assert_eq!(shown(&date, &every(0, 1, 0), true), "2019-12-31 00:00:00");
        assert_eq!(
            shown(&date, &every(0, 0, 90 * MICROS_PER_MINUTE), false),
            "2020-01-01 01:30:00"
        );
        assert_eq!(shown(&date, &every(0, 0, 0), false), "2020-01-01 00:00:00");
        let end = day(2020, 1, 31);
        assert_eq!(shown(&end, &every(1, 0, 0), false), "2020-02-29 00:00:00");
        assert_eq!(shown(&day(2020, 3, 31), &every(1, 0, 0), true), "2020-02-29 00:00:00");
        assert_eq!(shown(&day(2019, 2, 28), &every(12, 0, 0), false), "2020-02-28 00:00:00");
        assert_eq!(shown(&day(2020, 2, 29), &every(12, 0, 0), false), "2021-02-28 00:00:00");
        assert_eq!(shown(&day(0, 1, 1), &every(0, 1, 0), false), "0001-01-02 (BC) 00:00:00");
        let ten = stamp(2020, 1, 1, 10 * MICROS_PER_HOUR);
        assert_eq!(shown(&ten, &every(0, 0, 90 * MICROS_PER_MINUTE), true), "2020-01-01 08:30:00");
        assert_eq!(
            shown(&stamp(2020, 1, 31, 10 * MICROS_PER_HOUR), &every(1, 0, 0), false),
            "2020-02-29 10:00:00"
        );
        assert_eq!(
            shown(&stamp(2020, 1, 1, 0), &every(0, 0, -1), false),
            "2019-12-31 23:59:59.999999"
        );
    }

    /// The months land first, then the days, then the microseconds.
    ///
    /// The order only shows when a month lands on a day that does not exist, which is what both of
    /// these are. A month and a day added to the thirty first of January is the first of March,
    /// because the clamp to the twenty ninth of February happens before the day is added, and a
    /// month applied twice over would be the second.
    #[test]
    fn the_three_fields_are_applied_in_the_order_they_are_written_in() {
        assert_eq!(shown(&day(2020, 1, 31), &every(1, 1, 0), false), "2020-03-01 00:00:00");
        assert_eq!(
            shown(
                &stamp(2020, 1, 31, 23 * MICROS_PER_HOUR),
                &every(1, 0, 2 * MICROS_PER_HOUR),
                false
            ),
            "2020-03-01 01:00:00"
        );
    }

    /// A time is a clock and not a point in history, so it wraps and the whole days go nowhere.
    #[test]
    fn a_time_wraps_at_midnight_and_keeps_only_the_microseconds() {
        let ten = Value::Time(10 * MICROS_PER_HOUR);
        assert_eq!(shown(&ten, &every(0, 0, MICROS_PER_HOUR), false), "11:00:00");
        assert_eq!(shown(&every(0, 0, MICROS_PER_HOUR), &ten, false), "11:00:00");
        assert_eq!(shown(&ten, &every(1, 0, 0), false), "10:00:00");
        assert_eq!(shown(&ten, &every(0, -1, MICROS_PER_HOUR), false), "11:00:00");
        assert_eq!(shown(&ten, &every(0, 0, i64::MAX), false), "14:00:54.775807");
        let late = Value::Time(23 * MICROS_PER_HOUR + 30 * MICROS_PER_MINUTE);
        assert_eq!(shown(&late, &every(0, 0, MICROS_PER_HOUR), false), "00:30:00");
        let early = Value::Time(30 * MICROS_PER_MINUTE);
        assert_eq!(shown(&early, &every(0, 0, MICROS_PER_HOUR), true), "23:30:00");
    }

    /// Three ways out of range and three sentences, which are upstream's three.
    ///
    /// The months and the days each have a range of their own to leave, and the answer has the
    /// timestamp range on top of both, so a shift that stays inside the calendar can still be a
    /// moment that cannot be written down.
    #[test]
    fn each_way_out_of_range_says_what_upstream_says() {
        let date = day(2020, 1, 1);
        let error = shift(&date, &every(i32::MIN, 0, 0), false).expect_err("no such year");
        assert_eq!(error.to_string(), "Conversion Error: Date out of range: -178954951-5-1");
        let error = shift(&date, &every(0, i32::MAX, 0), false).expect_err("no such day");
        assert_eq!(error.to_string(), "Out of Range Error: Date out of range");
        let error = shift(&date, &every(0, 0, i64::MAX), false).expect_err("no such moment");
        assert_eq!(error.to_string(), "Conversion Error: Date and time not in timestamp range");
        let newest = day(294_247, 1, 10);
        let error = shift(&newest, &every(0, 1, 0), false).expect_err("no such moment");
        assert_eq!(error.to_string(), "Conversion Error: Date and time not in timestamp range");
        // The newest date there is, which is a long way past the newest moment there is, so it is
        // the moment that is reported and not the day even though a day was what was added.
        let far = Value::Date(i32::MAX - 1);
        let error = shift(&far, &every(0, 1, 0), false).expect_err("no such moment");
        assert_eq!(error.to_string(), "Conversion Error: Date and time not in timestamp range");
        assert_eq!(shown(&day(294_247, 1, 9), &every(0, 1, 0), false), "294247-01-10 00:00:00");
    }

    /// The two ends of the timestamp range, which are not the two ends of the `i64` that holds it.
    #[test]
    fn the_timestamp_range_stops_one_short_of_the_infinity_above_it() {
        assert_eq!(NEWEST_TIMESTAMP, i64::MAX - 1);
        assert_eq!(OLDEST_TIMESTAMP, i64::from(days_from_civil(-290_308, 12, 22)) * MICROS_PER_DAY);
        assert_eq!(Value::Timestamp(NEWEST_TIMESTAMP).to_string(), "294247-01-10 04:00:54.775806");
        assert_eq!(Value::Timestamp(OLDEST_TIMESTAMP).to_string(), "290309-12-22 (BC) 00:00:00");
    }

    /// Every one of these is what the DuckDB binary on `server3` answered for this timestamp.
    #[test]
    fn every_part_of_a_timestamp_is_what_duckdb_says_it_is() {
        let wanted = [
            ("year", 2024),
            ("month", 2),
            ("day", 29),
            ("hour", 13),
            ("minute", 45),
            ("second", 59),
            ("millisecond", 59_654),
            ("microsecond", 59_654_321),
            ("week", 9),
            ("quarter", 1),
            ("dayofweek", 4),
            ("isodow", 4),
            ("dayofyear", 60),
            ("decade", 202),
            ("century", 21),
            ("millennium", 3),
            ("era", 1),
            ("isoyear", 2024),
            ("yearweek", 202_409),
        ];
        for (spelling, answer) in wanted {
            let found = part(spelling).of_micros(moment()).expect("a part of a timestamp");
            assert_eq!(found, answer, "date_part('{spelling}', ...)");
        }
    }

    #[test]
    fn every_truncation_of_a_timestamp_is_what_duckdb_says_it_is() {
        let at = |year, month, day, hours: i64, minutes: i64, seconds: i64, micros: i64| {
            i64::from(days_from_civil(year, month, day)) * MICROS_PER_DAY
                + hours * MICROS_PER_HOUR
                + minutes * MICROS_PER_MINUTE
                + seconds * MICROS_PER_SECOND
                + micros
        };
        let wanted = [
            ("year", at(2024, 1, 1, 0, 0, 0, 0)),
            ("month", at(2024, 2, 1, 0, 0, 0, 0)),
            ("day", at(2024, 2, 29, 0, 0, 0, 0)),
            ("hour", at(2024, 2, 29, 13, 0, 0, 0)),
            ("minute", at(2024, 2, 29, 13, 45, 0, 0)),
            ("second", at(2024, 2, 29, 13, 45, 59, 0)),
            ("millisecond", at(2024, 2, 29, 13, 45, 59, 654_000)),
            ("microsecond", at(2024, 2, 29, 13, 45, 59, 654_321)),
            ("week", at(2024, 2, 26, 0, 0, 0, 0)),
            ("quarter", at(2024, 1, 1, 0, 0, 0, 0)),
            ("decade", at(2020, 1, 1, 0, 0, 0, 0)),
            ("century", at(2000, 1, 1, 0, 0, 0, 0)),
            ("millennium", at(2000, 1, 1, 0, 0, 0, 0)),
            ("isoyear", at(2024, 1, 1, 0, 0, 0, 0)),
            ("yearweek", at(2024, 2, 26, 0, 0, 0, 0)),
            ("epoch", at(2024, 2, 29, 13, 45, 59, 0)),
        ];
        for (spelling, answer) in wanted {
            let found = part(spelling).truncate_micros(moment()).expect("a truncation");
            assert_eq!(found, answer, "date_trunc('{spelling}', ...)");
        }
    }

    /// A date has no time in it and DuckDB answers zero rather than refusing, which matters because
    /// `EventDate` in ClickBench is a date and `EventTime` is a timestamp.
    #[test]
    fn the_time_parts_of_a_date_are_zero() {
        let days = days_from_civil(2013, 7, 15);
        for spelling in ["hour", "minute", "second", "millisecond", "microsecond"] {
            assert_eq!(part(spelling).of_days(days).expect("a part of a date"), 0, "{spelling}");
        }
        assert_eq!(part("day").of_days(days).expect("a part of a date"), 15);
    }

    /// The four rows read off DuckDB for the parts that are off by one around the turn of a
    /// century, which are the ones worth holding to because every one of them looks wrong.
    #[test]
    fn the_turn_of_a_century_is_counted_the_way_duckdb_counts_it() {
        let wanted = [
            (2000, 6, 1, 20, 2000, 2, 2000, 200, 2000),
            (2021, 1, 1, 21, 2000, 3, 2000, 202, 2020),
            (1999, 12, 31, 20, 1900, 2, 1000, 199, 1990),
            (1970, 1, 1, 20, 1900, 2, 1000, 197, 1970),
        ];
        for (year, month, day, century, at_century, millennium, at_millennium, decade, at_decade) in
            wanted
        {
            let days = days_from_civil(year, month, day);
            let of = |spelling: &str| part(spelling).of_days(days).expect("a part of a date");
            let start = |spelling: &str| {
                let truncated = part(spelling).truncate_days(days).expect("a truncation");
                civil_from_days(truncated).0
            };
            assert_eq!(of("century"), century, "century of {year}");
            assert_eq!(start("century"), at_century, "century start of {year}");
            assert_eq!(of("millennium"), millennium, "millennium of {year}");
            assert_eq!(start("millennium"), at_millennium, "millennium start of {year}");
            assert_eq!(of("decade"), decade, "decade of {year}");
            assert_eq!(start("decade"), at_decade, "decade start of {year}");
        }
    }

    /// Year zero and the years before it, where the century count skips a year that the calendar
    /// has and the printed date is a year off the stored one.
    #[test]
    fn a_year_before_year_one_counts_backwards_the_way_duckdb_does() {
        for (year, century, millennium, decade, era) in
            [(-46, -1, -1, -4, 0), (1, 1, 1, 0, 1), (0, -1, -1, 0, 0)]
        {
            let days = days_from_civil(year, 6, 1);
            let of = |spelling: &str| part(spelling).of_days(days).expect("a part of a date");
            assert_eq!(of("century"), century, "century of {year}");
            assert_eq!(of("millennium"), millennium, "millennium of {year}");
            assert_eq!(of("decade"), decade, "decade of {year}");
            assert_eq!(of("era"), era, "era of {year}");
        }
    }

    /// 1 January 2021 is in week 53 of 2020, which is the case every home grown week number gets
    /// wrong, so it is the case worth having a test for.
    #[test]
    fn a_week_belongs_to_the_year_its_thursday_is_in() {
        for (year, month, day, iso_year, week) in [
            (2021, 1, 1, 2020, 53),
            (2024, 2, 29, 2024, 9),
            (2024, 2, 25, 2024, 8),
            (1970, 1, 1, 1970, 1),
            (1999, 12, 31, 1999, 52),
        ] {
            let days = days_from_civil(year, month, day);
            assert_eq!(iso_week(days), (iso_year, week), "{year}-{month}-{day}");
        }
    }

    /// Sunday is 0 to `dayofweek` and 7 to `isodow`, which is two numberings for one question and
    /// the reason both are in the enum.
    #[test]
    fn the_two_weekday_numberings_disagree_about_sunday() {
        let sunday = days_from_civil(2024, 2, 25);
        assert_eq!(part("dayofweek").of_days(sunday).expect("a weekday"), 0);
        assert_eq!(part("isodow").of_days(sunday).expect("a weekday"), 7);
    }

    #[test]
    fn a_specifier_that_is_not_one_says_so_the_way_duckdb_does() {
        let error = Part::parse("qtr").expect_err("qtr is not a specifier");
        assert_eq!(error.to_string(), "Conversion Error: extract specifier \"qtr\" not recognized");
    }

    #[test]
    fn a_specifier_is_read_whatever_case_it_is_written_in() {
        assert_eq!(Part::parse("MINUTE").expect("a part"), Part::Minute);
        assert_eq!(Part::parse("Minute").expect("a part"), Part::Minute);
        assert_eq!(Part::parse("mins").expect("a part"), Part::Minute);
    }

    /// The two DuckDB refuses, and it refuses them at different places for different reasons, so
    /// neither message is invented here.
    #[test]
    fn the_parts_with_no_answer_say_which_answer_is_missing() {
        let error = part("epoch").of_micros(moment()).expect_err("epoch is a double");
        assert!(error.to_string().contains("double"), "{error}");
        let error = part("era").truncate_micros(moment()).expect_err("an era does not truncate");
        assert!(error.to_string().contains("DATETRUNC"), "{error}");
    }

    /// Before the epoch every one of these is a subtraction that a truncation towards zero gets
    /// wrong by a whole unit, so the floor is worth a test of its own.
    #[test]
    fn a_time_before_the_epoch_truncates_downwards() {
        let moment = -MICROS_PER_SECOND - 1;
        assert_eq!(part("second").truncate_micros(moment).expect("a truncation"), -2_000_000);
        assert_eq!(part("day").truncate_micros(moment).expect("a truncation"), -MICROS_PER_DAY);
        assert_eq!(part("second").of_micros(moment).expect("a part"), 58);
        assert_eq!(part("day").of_micros(moment).expect("a part"), 31);
    }

    fn built(name: &str, count: i128) -> (i32, i32, i64) {
        interval(name, Count::Whole(count)).expect("an interval")
    }

    /// A unit lands in one field and nothing converts between them, so a week is days and an hour
    /// is microseconds and neither of them is ever a month.
    #[test]
    fn every_unit_lands_in_the_one_field_that_holds_it() {
        assert_eq!(built("to_years", 1), (12, 0, 0));
        assert_eq!(built("to_months", 13), (13, 0, 0));
        assert_eq!(built("to_quarters", 5), (15, 0, 0));
        assert_eq!(built("to_decades", 1), (120, 0, 0));
        assert_eq!(built("to_centuries", -1), (-1_200, 0, 0));
        assert_eq!(built("to_millennia", 1), (12_000, 0, 0));
        assert_eq!(built("to_days", 1), (0, 1, 0));
        assert_eq!(built("to_weeks", 3), (0, 21, 0));
        assert_eq!(built("to_hours", 25), (0, 0, 25 * MICROS_PER_HOUR));
        assert_eq!(built("to_minutes", -90), (0, 0, -90 * MICROS_PER_MINUTE));
        assert_eq!(built("to_microseconds", 1_500_000), (0, 0, 1_500_000));
    }

    /// The two that take a DOUBLE keep what is after the point, which is the only reason they are
    /// not integer functions like the other eleven.
    #[test]
    fn seconds_and_milliseconds_keep_their_fraction() {
        let real = |name, count| interval(name, Count::Real(count)).expect("an interval");
        assert_eq!(real("to_seconds", 2.7), (0, 0, 2_700_000));
        assert_eq!(real("to_milliseconds", 1.5), (0, 0, 1_500));
        assert_eq!(real("to_seconds", -100.0), (0, 0, -100_000_000));
    }

    /// Upstream's own sentence, which names the unit that was asked for rather than the field the
    /// count landed in, and prints a DOUBLE count with six digits after the point.
    #[test]
    fn a_count_that_does_not_fit_says_which_unit_it_was_counting() {
        let error = interval("to_years", Count::Whole(2_147_483_647)).expect_err("out of range");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Interval value 2147483647 years out of range"
        );
        let error = interval("to_hours", Count::Whole(i64::MAX.into())).expect_err("out of range");
        assert!(error.to_string().ends_with("9223372036854775807 hours out of range"), "{error}");
        let error = interval("to_seconds", Count::Real(1e30)).expect_err("out of range");
        assert!(
            error.to_string().contains("1000000000000000019884624838656.000000 seconds"),
            "{error}"
        );
    }
}
