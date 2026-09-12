//! The calendar, which is what `date_part` and `date_trunc` are made of.
//!
//! Both functions are one string and one moment or length, and the string decides which of twenty
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
//! Most parts are whole numbers, but `epoch` and `julian` are not, so each reader here has a double
//! twin next to it and the binder decides which one a query gets. What is missing is `timezone`,
//! `timezone_hour` and `timezone_minute`, which need a session time zone before they mean anything.

use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};

use crate::cast;
use crate::scalar::{Op, negation_overflow, overflow};

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
    /// Seconds since 1970, with the fraction, which is why this part is a double.
    Epoch,
    /// Days since noon on 24 November 4714 BC, with the fraction, which is the other double.
    ///
    /// The count is off by half a day from the way it is usually written, since the astronomical
    /// day starts at noon and DuckDB's starts at midnight. `date_part('julian', DATE '2020-01-01')`
    /// is 2458850 where an almanac says the Julian day of that midnight is 2458849.5, measured.
    Julian,
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
    ("julian", Part::Julian),
    ("jd", Part::Julian),
];

/// The day the Julian count starts, as a count of days from 1970, which is the number that turns
/// one into the other.
const JULIAN_AT_EPOCH: i64 = 2_440_588;

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
            // The two that carry a fraction are doubles, and the binder knows it: a call that asks
            // for either of them is a double call and never reaches this. Reaching it anyway is a
            // bug in the binder rather than a query anybody wrote.
            Self::Epoch | Self::Julian => {
                return Err(Error::internal(format!("{self:?} is a double and this is not")));
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

    /// The part of a date as a double.
    ///
    /// Every part can be asked for as a double, not only the two that are one. `date_part(p, d)`
    /// over a column of specifiers is a double call, since nothing at binding time knows what the
    /// column holds, and it still has to answer `year` when a row says year. So this is the whole
    /// number widened for every part but the two that carry a fraction.
    ///
    /// # Errors
    ///
    /// If the part is one a date does not have.
    pub(crate) fn double_of_days(self, days: i32) -> Result<f64> {
        Ok(match self {
            Self::Epoch => f64::from(days) * 86_400.0,
            Self::Julian => f64::from(days) + JULIAN_AT_EPOCH as f64,
            // A part of a date is a year at the widest, so this is exact for every value a date
            // can hold, unlike the microsecond arithmetic next to it.
            _ => self.of_days(days)? as f64,
        })
    }

    /// The part of a timestamp as a double.
    ///
    /// The two fractional parts are worked out in floating point from the microseconds, which is
    /// where the last digits of the widest timestamps come from: the microseconds of a moment past
    /// the year 200000 do not fit a double exactly, so the Julian day of one comes back as
    /// 107754599.99998842 rather than a round number. That is upstream's answer as well, because it
    /// is upstream's arithmetic in the same order.
    ///
    /// # Errors
    ///
    /// If the part is one a timestamp does not have, or if it needs the date and the timestamp is
    /// outside the range a date covers.
    pub(crate) fn double_of_micros(self, micros: i64) -> Result<f64> {
        Ok(match self {
            Self::Epoch => micros as f64 / MICROS_PER_SECOND as f64,
            Self::Julian => micros as f64 / MICROS_PER_DAY as f64 + JULIAN_AT_EPOCH as f64,
            _ => self.of_micros(micros)? as f64,
        })
    }

    /// The part of an interval as a double.
    ///
    /// A year is three hundred and sixty five and a quarter days here and a month is thirty of
    /// them, which is how upstream turns a length with months in it into a count of seconds.
    /// `INTERVAL '1 year'` is 31557600 seconds and `INTERVAL '12 months'` is the same, while
    /// `INTERVAL '11 months'` is 28512000, so the years are counted first and the leftover months
    /// after them. All measured.
    ///
    /// # Errors
    ///
    /// If the part is one an interval does not have, which the caller was supposed to have refused
    /// already.
    pub(crate) fn double_of_interval(self, months: i32, days: i32, micros: i64) -> Result<f64> {
        if self == Self::Epoch {
            let years = f64::from(months / 12) * 31_557_600.0;
            let rest = f64::from(months % 12) * 2_592_000.0;
            let days = f64::from(days) * 86_400.0;
            return Ok(years + rest + days + micros as f64 / MICROS_PER_SECOND as f64);
        }
        Ok(self.of_interval(months, days, micros)? as f64)
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
            | Self::Epoch
            | Self::Julian => days,
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

    /// This part again, if an interval is made of it.
    ///
    /// An interval is three counts and not a moment, so the parts that need a calendar have no
    /// answer over one. Upstream refuses those at run time rather than at binding, with a sentence
    /// that names the type and echoes the specifier as it was written, which is why this takes the
    /// spelling as well as the part it was already parsed into.
    ///
    /// # Errors
    ///
    /// If the part is one an interval does not have, which is every part that has to look at a
    /// calendar to mean anything: the week, the three ways of numbering a day, the ISO year and the
    /// era. A time zone is refused here as well, one step before the fact that we have none.
    pub(crate) fn of_an_interval(self, spelling: &str) -> Result<Self> {
        if self.fits_an_interval() {
            Ok(self)
        } else {
            Err(Error::not_implemented(format!("\"interval\" units \"{spelling}\" not recognized")))
        }
    }

    /// Whether an interval is made of this part.
    ///
    /// The list lives here and nowhere else, so the check in [`Part::of_an_interval`] and the
    /// arithmetic in [`Part::of_interval`] cannot come apart.
    fn fits_an_interval(self) -> bool {
        matches!(
            self,
            Self::Year
                | Self::Month
                | Self::Day
                | Self::Hour
                | Self::Minute
                | Self::Second
                | Self::Millisecond
                | Self::Microsecond
                | Self::Decade
                | Self::Century
                | Self::Millennium
                | Self::Quarter
                | Self::Epoch
        )
    }

    /// The part of an interval, which is a count of months, a count of days and a count of
    /// microseconds.
    ///
    /// The three counts stay apart here the way they stay apart everywhere else, so this reads one
    /// field and never converts between them. `date_part('day', INTERVAL '36 hours')` is zero and
    /// `date_part('hour', INTERVAL '36 hours')` is thirty six, because nothing carries the hours up
    /// into a day and nothing is going to.
    ///
    /// The years and everything longer divide the months, and the division truncates towards zero
    /// rather than flooring, so minus eleven months is zero years and minus fourteen is minus one.
    /// The minutes and everything shorter take the remainder of the microseconds the way the clock
    /// parts of a timestamp do, carrying the seconds into the milliseconds, but the hours do not,
    /// since there is no day above them to roll into.
    ///
    /// # Errors
    ///
    /// If the part is `epoch`, which is a double, and if it is one an interval does not have, which
    /// the caller was supposed to have refused already.
    pub(crate) fn of_interval(self, months: i32, days: i32, micros: i64) -> Result<i64> {
        let months = i64::from(months);
        Ok(match self {
            Self::Year => months / 12,
            Self::Month => months % 12,
            Self::Day => i64::from(days),
            Self::Hour => micros / MICROS_PER_HOUR,
            Self::Minute => micros / MICROS_PER_MINUTE % 60,
            Self::Second => micros / MICROS_PER_SECOND % 60,
            Self::Millisecond => micros / 1_000 % 60_000,
            Self::Microsecond => micros % 60_000_000,
            Self::Decade => months / 120,
            Self::Century => months / 1_200,
            Self::Millennium => months / 12_000,
            // The quarter of a whole year is the first one, and the quarter of a negative count of
            // months is measured the same way, so eleven months is the fourth quarter and minus
            // five months is the zeroth. It is upstream's arithmetic rather than a number anybody
            // would name.
            Self::Quarter => months % 12 / 3 + 1,
            // A double, the same as it is over a date, so a call that asks for it is a double call
            // and lands in `double_of_interval` rather than here.
            Self::Epoch => {
                return Err(Error::internal(format!("{self:?} is a double and this is not")));
            }
            _ => {
                return Err(Error::internal(format!(
                    "an interval has no {self:?} in it and the caller did not check"
                )));
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

    /// An interval truncated to the part.
    ///
    /// A truncation clears every field below the part and leaves the ones above it alone, so an
    /// interval truncated to a month keeps its months and loses its days and its microseconds. The
    /// field the part lands in is cut down to a whole number of the part towards zero, which is
    /// where this and the timestamp above it disagree: a timestamp floors, so that a moment before
    /// the epoch lands on the boundary below it, and a length has no epoch to be before.
    ///
    /// Every part an interval does not have is still answered here, unlike [`Part::of_interval`],
    /// because a truncation to a part a length has none of is the length with that part and
    /// everything under it cleared. So a week truncates the days to a multiple of seven and a day
    /// of the week does nothing at all, both measured.
    ///
    /// # Errors
    ///
    /// If the part is an era, with the sentence upstream uses for a length, which is the one it
    /// uses for a moment with the last word taken off.
    pub(crate) fn truncate_interval(
        self,
        months: i32,
        days: i32,
        micros: i64,
    ) -> Result<(i32, i32, i64)> {
        let whole = |unit: i32| months - months % unit;
        let clipped = |unit: i64| micros - micros % unit;
        Ok(match self {
            Self::Millennium => (whole(12_000), 0, 0),
            Self::Century => (whole(1_200), 0, 0),
            Self::Decade => (whole(120), 0, 0),
            Self::Year | Self::IsoYear => (whole(12), 0, 0),
            Self::Quarter => (whole(3), 0, 0),
            Self::Month => (months, 0, 0),
            Self::Week | Self::YearWeek => (months, days - days % 7, 0),
            Self::Day | Self::DayOfWeek | Self::IsoDayOfWeek | Self::DayOfYear | Self::Julian => {
                (months, days, 0)
            }
            Self::Hour => (months, days, clipped(MICROS_PER_HOUR)),
            Self::Minute => (months, days, clipped(MICROS_PER_MINUTE)),
            Self::Second | Self::Epoch => (months, days, clipped(MICROS_PER_SECOND)),
            Self::Millisecond => (months, days, clipped(1_000)),
            Self::Microsecond => (months, days, micros),
            Self::Era => {
                return Err(Error::not_implemented("Specifier type not implemented for DATETRUNC"));
            }
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

/// Two intervals added or taken apart, which is field by field and not by length.
///
/// The order over intervals says a month is thirty days, but the arithmetic does not, because the
/// three fields mean different things once a date is involved. `INTERVAL '1 month' + INTERVAL '30
/// days'` is one month and thirty days and not two months.
///
/// The two directions fail differently, which was measured rather than assumed. Addition reports
/// the plain integer overflow, naming the physical type and the two counts, and subtraction has a
/// sentence of its own that names the field instead.
pub(crate) fn combine(left: &Value, right: &Value, subtract: bool) -> Result<Value> {
    let (
        Value::Interval { months: a_months, days: a_days, micros: a_micros },
        Value::Interval { months: b_months, days: b_days, micros: b_micros },
    ) = (left, right)
    else {
        return Err(Error::internal(format!("{left} and {right} are not two intervals")));
    };
    if subtract {
        return Ok(Value::Interval {
            months: a_months.checked_sub(*b_months).ok_or_else(|| short("months"))?,
            days: a_days.checked_sub(*b_days).ok_or_else(|| short("days"))?,
            micros: a_micros.checked_sub(*b_micros).ok_or_else(|| short("micros"))?,
        });
    }
    let whole = |a: i32, b: i32| {
        a.checked_add(b).ok_or_else(|| {
            overflow(Op::Add, &LogicalType::Integer, &Value::Integer(a), &Value::Integer(b))
        })
    };
    Ok(Value::Interval {
        months: whole(*a_months, *b_months)?,
        days: whole(*a_days, *b_days)?,
        micros: a_micros.checked_add(*b_micros).ok_or_else(|| {
            overflow(
                Op::Add,
                &LogicalType::BigInt,
                &Value::BigInt(*a_micros),
                &Value::BigInt(*b_micros),
            )
        })?,
    })
}

/// An interval with a minus in front of it, which negates all three fields.
pub(crate) fn negated(value: &Value) -> Result<Value> {
    let Value::Interval { months, days, micros } = value else {
        return Err(Error::internal(format!("{value} is not an interval")));
    };
    Ok(Value::Interval {
        months: months.checked_neg().ok_or_else(negation_overflow)?,
        days: days.checked_neg().ok_or_else(negation_overflow)?,
        micros: micros.checked_neg().ok_or_else(negation_overflow)?,
    })
}

/// Whether a pair of values is an interval being scaled by a number.
///
/// The number is a whole one or a double and nothing else, because those are the two the signature
/// casts to, and which of the two it is decides which arithmetic happens.
pub(crate) fn is_scale(left: &Value, right: &Value) -> bool {
    let interval = |value: &Value| matches!(value, Value::Interval { .. });
    let number = |value: &Value| matches!(value, Value::BigInt(_) | Value::Double(_));
    (interval(left) && number(right)) || (number(left) && interval(right))
}

/// An interval times a number or divided by one.
///
/// There are two multiplications and one division. A whole number multiplies the three fields as
/// they are and nothing moves between them, so `INTERVAL '1 month' * 3` is three months. A number
/// with a point in it goes through [`spread`], and dividing always does, which is why
/// `INTERVAL '1 month' / 3` is ten days rather than nothing at all.
///
/// # Errors
///
/// If a field does not fit what holds it, in one of the three sentences upstream has for it.
pub(crate) fn scaled(left: &Value, right: &Value, divide: bool) -> Result<Value> {
    let (months, days, micros, factor) = match (left, right) {
        (Value::Interval { months, days, micros }, factor)
        | (factor, Value::Interval { months, days, micros }) => (*months, *days, *micros, factor),
        _ => return Err(Error::internal(format!("{left} and {right} are not a scale"))),
    };
    match factor {
        Value::BigInt(count) => whole(months, days, micros, *count),
        Value::Double(factor) => spread(months, days, micros, *factor, divide),
        other => Err(Error::internal(format!("{other} does not scale an interval"))),
    }
}

/// An interval times a whole number, which is three multiplications and no arithmetic between them.
///
/// The count arrives as a `BIGINT` because that is what the overload takes, and the first thing
/// upstream does with it is narrow it to the width a month count is held in, so a factor of ten
/// billion is a cast failure and not an overflow.
fn whole(months: i32, days: i32, micros: i64, count: i64) -> Result<Value> {
    let count = cast::narrow(count)?;
    let field = |value: i32| {
        value.checked_mul(count).ok_or_else(|| {
            overflow(
                Op::Multiply,
                &LogicalType::Integer,
                &Value::Integer(value),
                &Value::Integer(count),
            )
        })
    };
    Ok(Value::Interval {
        months: field(months)?,
        days: field(days)?,
        micros: micros.checked_mul(i64::from(count)).ok_or_else(|| {
            overflow(
                Op::Multiply,
                &LogicalType::BigInt,
                &Value::BigInt(micros),
                &Value::BigInt(i64::from(count)),
            )
        })?,
    })
}

/// An interval scaled by a double, where what is left over on a field moves down to the next one.
///
/// The lengths the leftovers move at are the ones the order over intervals uses, thirty days to a
/// month and twenty four hours to a day, so half of a month is fifteen days and a third of a day is
/// eight hours. Neither is a calendar answer and both are upstream's.
///
/// The month leftover is the one piece of this that is not plain arithmetic. Upstream turns it into
/// a whole number of millionths of a day before it becomes microseconds, so `INTERVAL '1 month' / 7`
/// is `4 days 06:51:25.6896` and not the `06:51:25.714286` the exact division would give. The day
/// leftover has no such step, which is why `INTERVAL '3 days' / 7` does end in `.571429`. It reads
/// like a mistake upstream, and it is copied here on purpose, because a query that answers one way
/// on one engine and another way on the other is the bug we are paid to not have.
///
/// The microseconds round half to even at the end, so half a microsecond is none and two and a half
/// are two.
#[expect(
    clippy::cast_precision_loss,
    reason = "upstream scales in a double as well, so the same digits go missing on both engines"
)]
fn spread(months: i32, days: i32, micros: i64, factor: f64, divide: bool) -> Result<Value> {
    let apply = |field: f64| if divide { field / factor } else { field * factor };
    let months = apply(f64::from(months));
    let days = apply(f64::from(days));
    let micros = apply(micros as f64);
    let millionths = (months.fract() * 30.0 * 1e6).round_ties_even();
    let day = MICROS_PER_DAY as f64;
    let spilled = millionths * (day / 1e6) + days.fract() * day;
    let carried = (spilled / day).trunc();
    Ok(Value::Interval {
        months: field(months.trunc(), divide)?,
        days: field(days.trunc() + carried, divide)?,
        micros: moment(micros + spilled - carried * day, divide)?,
    })
}

/// The sentence for a subtraction that leaves a field with no room, which names the field.
fn short(field: &str) -> Error {
    Error::out_of_range(format!("Interval {field} subtraction out of range"))
}

/// A whole double back into a month or a day count.
#[expect(clippy::cast_possible_truncation, reason = "the range is checked before the cast")]
fn field(value: f64, divide: bool) -> Result<i32> {
    if value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX) {
        Ok(value as i32)
    } else {
        Err(too_big(divide))
    }
}

/// The same for the microseconds, where the top of the range is written out because `i64::MAX` as a
/// double is the power of two above it and a cast at that value saturates rather than failing.
#[expect(clippy::cast_possible_truncation, reason = "the range is checked before the cast")]
fn moment(value: f64, divide: bool) -> Result<i64> {
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    let value = value.round_ties_even();
    if (-LIMIT..LIMIT).contains(&value) { Ok(value as i64) } else { Err(too_big(divide)) }
}

/// Whether a pair of values is a date with a count of days next to it.
///
/// The count is an `INTEGER` and nothing else, because the signature casts it to one and refuses
/// every width that does not fit there, so this only has to tell the pair apart from two numbers.
pub(crate) fn is_counted(left: &Value, right: &Value) -> bool {
    matches!(
        (left, right),
        (Value::Date(_), Value::Integer(_)) | (Value::Integer(_), Value::Date(_))
    )
}

/// A date with a count of days added to it or taken off it, which is still a date.
///
/// The one shape of date arithmetic that answers a date rather than a timestamp, since a count of
/// days carries no time of day. The range failure is the date one and not the timestamp one, which
/// is the other way round from a date with an interval on it, because nothing here turns the date
/// into a moment on the way past.
pub(crate) fn counted(left: &Value, right: &Value, subtract: bool) -> Result<Value> {
    let (day, count) = match (left, right) {
        (Value::Date(day), Value::Integer(count)) | (Value::Integer(count), Value::Date(day)) => {
            (*day, i64::from(*count))
        }
        _ => return Err(Error::internal(format!("{left} and {right} are not a date and a count"))),
    };
    Ok(Value::Date(shifted_days(day, 0, if subtract { -count } else { count })?))
}

/// One date taken off another, or one timestamp taken off another.
///
/// Two dates answer a count of days as a `BIGINT`, which is why the oldest date taken off the newest
/// one is a number and not a failure. Two timestamps answer an interval of days and microseconds,
/// never of months, because a month is not a length that a pair of moments can name.
///
/// The interval is worked out in microseconds first and that subtraction has to fit an `i64`, so the
/// widest pair of timestamps fails even though the interval it would name has room for the answer.
/// That is upstream's order and upstream's sentence.
pub(crate) fn apart(left: &Value, right: &Value) -> Result<Value> {
    match (left, right) {
        (Value::Date(late), Value::Date(early)) => {
            Ok(Value::BigInt(i64::from(*late) - i64::from(*early)))
        }
        (Value::Timestamp(late), Value::Timestamp(early)) => {
            let apart = late.checked_sub(*early).ok_or_else(too_far)?;
            // The day count of a difference that fits an `i64` of microseconds is about a hundred
            // million, so the narrowing cannot fail, and it reports the same sentence rather than
            // panicking if it ever does.
            let days = i32::try_from(apart / MICROS_PER_DAY).map_err(|_| too_far())?;
            Ok(Value::Interval { months: 0, days, micros: apart % MICROS_PER_DAY })
        }
        _ => Err(Error::internal(format!("{left} and {right} are not two of the same"))),
    }
}

fn too_far() -> Error {
    Error::conversion("Timestamp difference is out of bounds")
}

/// Whether a pair of values is a date and a time of day.
pub(crate) fn is_joined(left: &Value, right: &Value) -> bool {
    matches!((left, right), (Value::Date(_), Value::Time(_)) | (Value::Time(_), Value::Date(_)))
}

/// A date and a time of day as the one moment they name together.
///
/// A time runs to `24:00:00` rather than stopping below midnight, so the last hour of the range is a
/// timestamp on the next day, and there is nothing to wrap because the day comes from the date.
///
/// The sentence for a moment that does not fit is `Timestamp out of range` here and
/// `Date and time not in timestamp range` for a date with an interval on it. The two are different
/// paths upstream and they were measured rather than shared.
pub(crate) fn joined(left: &Value, right: &Value) -> Result<Value> {
    let (day, clock) = match (left, right) {
        (Value::Date(day), Value::Time(clock)) | (Value::Time(clock), Value::Date(day)) => {
            (*day, *clock)
        }
        _ => return Err(Error::internal(format!("{left} and {right} are not a date and a time"))),
    };
    let stamp = i128::from(day) * i128::from(MICROS_PER_DAY) + i128::from(clock);
    match i64::try_from(stamp) {
        Ok(stamp) if (OLDEST_TIMESTAMP..=NEWEST_TIMESTAMP).contains(&stamp) => {
            Ok(Value::Timestamp(stamp))
        }
        _ => Err(Error::out_of_range("Timestamp out of range")),
    }
}

/// One sentence each for the two operators, and the punctuation on the end of them is upstream's.
fn too_big(divide: bool) -> Error {
    if divide {
        Error::out_of_range("Overflow in INTERVAL division")
    } else {
        Error::out_of_range("Overflow in multiplication of INTERVAL.")
    }
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

    fn half() -> Value {
        Value::Double(0.5)
    }

    /// What two intervals added or taken apart print as.
    fn added(left: &Value, right: &Value, subtract: bool) -> String {
        combine(left, right, subtract).expect("the fields have room").to_string()
    }

    /// What a scaled interval prints as.
    fn times(interval: &Value, factor: &Value, divide: bool) -> String {
        scaled(interval, factor, divide).expect("the fields have room").to_string()
    }

    /// What a date with a count of days on it prints as.
    fn plus_days(date: &Value, count: i32, subtract: bool) -> String {
        counted(date, &Value::Integer(count), subtract).expect("in range").to_string()
    }

    /// What one date taken off another, or one timestamp taken off another, prints as.
    fn between(left: &Value, right: &Value) -> String {
        apart(left, right).expect("close enough together").to_string()
    }

    /// What a date with a time of day on it prints as.
    fn at(date: &Value, clock: i64) -> String {
        joined(date, &Value::Time(clock)).expect("in range").to_string()
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

    /// Two intervals keep their three fields apart, whatever the order over them says.
    #[test]
    fn two_intervals_add_and_subtract_one_field_at_a_time() {
        assert_eq!(added(&every(1, 0, 0), &every(0, 30, 0), false), "1 month 30 days");
        let left = every(1, 2, 3 * MICROS_PER_HOUR);
        let right = every(2, 1, MICROS_PER_HOUR);
        assert_eq!(added(&left, &right, true), "-1 month 1 day 02:00:00");
        assert_eq!(added(&left, &right, false), "3 months 3 days 04:00:00");
    }

    /// The two directions run out of room differently, which is upstream's doing and not ours.
    #[test]
    fn adding_reports_the_field_it_filled_and_subtracting_names_it() {
        let one = every(1, 1, 1);
        let full = every(i32::MAX, i32::MAX, i64::MAX);
        let error = combine(&full, &one, false).expect_err("no room for another month");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in addition of INT32 (2147483647 + 1)!"
        );
        let error =
            combine(&every(0, i32::MAX, 0), &one, false).expect_err("no room for another day");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in addition of INT32 (2147483647 + 1)!"
        );
        let error = combine(&every(0, 0, i64::MAX), &one, false).expect_err("no room for another");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in addition of INT64 (9223372036854775807 + 1)!"
        );
        let empty = every(i32::MIN, i32::MIN, i64::MIN);
        let error = combine(&empty, &one, true).expect_err("no room below");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Interval months subtraction out of range"
        );
        let error = combine(&every(0, i32::MIN, 0), &one, true).expect_err("no room below");
        assert_eq!(error.to_string(), "Out of Range Error: Interval days subtraction out of range");
        let error = combine(&every(0, 0, i64::MIN), &one, true).expect_err("no room below");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Interval micros subtraction out of range"
        );
    }

    /// A minus in front of an interval turns all three fields around.
    #[test]
    fn an_interval_negates_every_field_it_has() {
        let held = negated(&every(1, 2, 3 * MICROS_PER_HOUR)).expect("room to turn around");
        assert_eq!(held.to_string(), "-1 month -2 days -03:00:00");
        let error = negated(&every(i32::MIN, 0, 0)).expect_err("nothing to turn it into");
        assert_eq!(error.to_string(), "Out of Range Error: Overflow in negation of numeric value!");
    }

    /// A whole number multiplies the three fields where they are, so nothing moves between them.
    #[test]
    fn a_count_multiplies_each_field_where_it_stands() {
        assert_eq!(times(&every(1, 0, 0), &Value::BigInt(3), false), "3 months");
        assert_eq!(
            times(&every(1, 1, 1), &Value::BigInt(2), false),
            "2 months 2 days 00:00:00.000002"
        );
        assert_eq!(times(&every(0, 0, 25 * MICROS_PER_HOUR), &Value::BigInt(2), false), "50:00:00");
        assert_eq!(times(&every(1, 0, 0), &Value::BigInt(0), false), "00:00:00");
        assert_eq!(times(&every(1, 0, 0), &Value::BigInt(-1), false), "-1 month");
    }

    /// The count is narrowed before anything is multiplied, so a huge one is a cast failure.
    #[test]
    fn a_count_that_does_not_fit_a_month_count_fails_as_a_cast() {
        let error = scaled(&every(1, 0, 0), &Value::BigInt(10_000_000_000), false)
            .expect_err("no room in a month count");
        assert_eq!(
            error.to_string(),
            "Invalid Input Error: Type INT64 with value 10000000000 can't be cast because the value is out of range for the destination type INT32"
        );
        let error = scaled(&every(i32::MAX, 0, 0), &Value::BigInt(2), false).expect_err("too many");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in multiplication of INT32 (2147483647 * 2)!"
        );
        let error = scaled(&every(0, 0, i64::MAX), &Value::BigInt(2), false).expect_err("too many");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in multiplication of INT64 (9223372036854775807 * 2)!"
        );
    }

    /// Every row here is a statement that was run against the pinned binary for #393.
    ///
    /// The leftovers are what this is about. A month leftover is thirty days and a day leftover is
    /// twenty four hours, and the month one goes through a whole number of millionths of a day on
    /// the way, which is why a month in seven parts ends in `.6896` and three days in seven parts
    /// ends in `.571429`.
    #[test]
    fn a_number_with_a_point_in_it_moves_the_leftovers_down() {
        let month = every(1, 0, 0);
        assert_eq!(times(&month, &half(), false), "15 days");
        assert_eq!(times(&month, &Value::Double(7.0), true), "4 days 06:51:25.6896");
        assert_eq!(times(&month, &Value::Double(3.0), true), "10 days");
        assert_eq!(
            times(&every(1200, 0, 0), &Value::Double(7.0), true),
            "14 years 3 months 12 days 20:34:17.1552"
        );
        assert_eq!(times(&every(1, 1, 0), &Value::Double(7.0), true), "4 days 10:17:08.546743");
        assert_eq!(times(&every(-1, 1, 0), &Value::Double(7.0), true), "-4 days -03:25:42.832457");
        assert_eq!(times(&every(0, 3, 0), &Value::Double(7.0), true), "10:17:08.571429");
        assert_eq!(times(&every(1, 1, 0), &Value::Double(0.75), false), "23 days 06:00:00");
        let day_and_a_bit = every(0, 1, 30 * MICROS_PER_HOUR);
        assert_eq!(times(&day_and_a_bit, &half(), false), "27:00:00");
        let three_quarters = every(1, 1, 12 * MICROS_PER_HOUR);
        assert_eq!(times(&three_quarters, &Value::Double(0.75), false), "23 days 15:00:00");
        assert_eq!(
            times(&every(0, 0, 25 * MICROS_PER_HOUR), &Value::Double(1.5), false),
            "37:30:00"
        );
        let all_of_it = every(13, 1, MICROS_PER_HOUR + MICROS_PER_SECOND);
        assert_eq!(
            times(&all_of_it, &Value::Double(1.5), false),
            "1 year 7 months 16 days 13:30:01.5"
        );
    }

    /// The microseconds round half to even at the end, so a half of one is none of one.
    #[test]
    fn the_microseconds_round_half_to_even() {
        assert_eq!(times(&every(0, 0, 1), &half(), false), "00:00:00");
        assert_eq!(times(&every(0, 0, 3), &half(), false), "00:00:00.000002");
        assert_eq!(times(&every(0, 0, 5), &Value::Double(-0.5), false), "-00:00:00.000002");
        assert_eq!(times(&every(0, 0, 1), &Value::Double(2.5), false), "00:00:00.000002");
    }

    /// One sentence for a multiplication that does not fit and another for a division that does not.
    #[test]
    fn a_scaled_interval_that_does_not_fit_says_which_operator_it_was() {
        let full = every(i32::MAX, 0, 0);
        let error = scaled(&full, &Value::Double(2.0), false).expect_err("too many months");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in multiplication of INTERVAL."
        );
        let error = scaled(&full, &half(), true).expect_err("too many months");
        assert_eq!(error.to_string(), "Out of Range Error: Overflow in INTERVAL division");
        let day = every(0, 1, 0);
        let error = scaled(&day, &Value::Double(f64::NAN), false).expect_err("no such interval");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in multiplication of INTERVAL."
        );
        let error = scaled(&day, &Value::Double(f64::NAN), true).expect_err("no such interval");
        assert_eq!(error.to_string(), "Out of Range Error: Overflow in INTERVAL division");
        let error = scaled(&day, &Value::Double(f64::INFINITY), false).expect_err("no such one");
        assert_eq!(
            error.to_string(),
            "Out of Range Error: Overflow in multiplication of INTERVAL."
        );
        // Dividing by an infinity is nothing at all rather than a failure, since every field lands
        // on zero and zero fits.
        assert_eq!(times(&day, &Value::Double(f64::INFINITY), true), "00:00:00");
    }

    /// Every row here is a statement that was run against the pinned binary for #393.
    ///
    /// A count of days keeps the date a date, in either order for the addition, and the count that
    /// walks off the end of the calendar says so as a date and not as a timestamp.
    #[test]
    fn a_count_of_days_moves_a_date_and_leaves_it_a_date() {
        let date = day(2020, 1, 1);
        assert_eq!(plus_days(&date, 1, false), "2020-01-02");
        assert_eq!(plus_days(&date, 0, false), "2020-01-01");
        assert_eq!(plus_days(&date, -1, false), "2019-12-31");
        assert_eq!(plus_days(&date, 1, true), "2019-12-31");
        assert_eq!(
            counted(&Value::Integer(1), &date, false).expect("in range").to_string(),
            "2020-01-02"
        );
        assert_eq!(plus_days(&date, i32::MAX, true), "5877592-06-23 (BC)");
        let error = counted(&date, &Value::Integer(i32::MAX), false).expect_err("past the end");
        assert_eq!(error.to_string(), "Out of Range Error: Date out of range");
    }

    /// Two dates are a count and two timestamps are an interval, and neither answers in months.
    #[test]
    fn one_date_taken_off_another_is_days_and_one_timestamp_is_an_interval() {
        assert_eq!(between(&day(2020, 3, 1), &day(2020, 2, 1)), "29");
        assert_eq!(between(&day(2020, 2, 1), &day(2020, 3, 1)), "-29");
        assert_eq!(between(&day(2020, 1, 1), &day(2020, 1, 1)), "0");
        // The whole range, which is why the count is a `BIGINT` and not the `i32` a date is held in.
        assert_eq!(between(&Value::Date(NEWEST_DATE), &Value::Date(OLDEST_DATE)), "4294967292");
        let late = stamp(2020, 1, 2, 10 * MICROS_PER_HOUR);
        let early = stamp(2020, 1, 1, 8 * MICROS_PER_HOUR);
        assert_eq!(between(&late, &early), "1 day 02:00:00");
        assert_eq!(between(&early, &late), "-1 day -02:00:00");
        assert_eq!(between(&stamp(2020, 1, 1, 123_456), &stamp(2020, 1, 1, 0)), "00:00:00.123456");
    }

    /// The difference is worked out in microseconds, so the widest pair of moments has no answer.
    #[test]
    fn a_timestamp_difference_that_does_not_fit_an_i64_says_so() {
        let error = apart(&Value::Timestamp(OLDEST_TIMESTAMP), &stamp(2020, 1, 1, 0))
            .expect_err("too far apart");
        assert_eq!(error.to_string(), "Conversion Error: Timestamp difference is out of bounds");
    }

    /// A date and a time of day, with the range failure that is not the one an interval gets.
    #[test]
    fn a_date_and_a_time_are_the_one_moment_they_name() {
        let date = day(2020, 1, 1);
        assert_eq!(at(&date, 10 * MICROS_PER_HOUR), "2020-01-01 10:00:00");
        assert_eq!(
            joined(&Value::Time(10 * MICROS_PER_HOUR), &date).expect("in range").to_string(),
            "2020-01-01 10:00:00"
        );
        // A time runs to midnight inclusive, so the top of it is the day after.
        assert_eq!(at(&date, MICROS_PER_DAY), "2020-01-02 00:00:00");
        let error = joined(&Value::Date(NEWEST_DATE), &Value::Time(0)).expect_err("no such moment");
        assert_eq!(error.to_string(), "Out of Range Error: Timestamp out of range");
        let error = joined(&Value::Date(OLDEST_DATE), &Value::Time(0)).expect_err("no such moment");
        assert_eq!(error.to_string(), "Out of Range Error: Timestamp out of range");
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

    /// An era does not truncate and DuckDB says so in words worth copying. The two fractional
    /// parts do have an answer, but not a whole one, so asking the whole reader for it is a bug in
    /// the caller rather than something a query can reach.
    #[test]
    fn the_parts_with_no_whole_answer_say_which_reader_to_use() {
        let error = part("epoch").of_micros(moment()).expect_err("epoch is a double");
        assert!(error.to_string().contains("double"), "{error}");
        let error = part("julian").of_days(0).expect_err("a julian day is a double");
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

    /// The two parts that carry a fraction, over a moment and over a day, against the numbers the
    /// binary prints. The last case is far enough out that a julian day no longer fits a double to
    /// the microsecond, and the digits that fall off there are upstream's digits as well.
    #[test]
    fn the_two_fractional_parts_count_seconds_and_days() {
        let day = days_from_civil(2020, 1, 1);
        let epoch = |micros| part("epoch").double_of_micros(micros).expect("seconds");
        let julian = |micros| part("julian").double_of_micros(micros).expect("days");
        assert_eq!(epoch(i64::from(day) * MICROS_PER_DAY + 500_000), 1_577_836_800.5);
        assert_eq!(epoch(-500_000), -0.5);
        assert_eq!(julian(i64::from(day) * MICROS_PER_DAY + 6 * MICROS_PER_HOUR), 2_458_850.25);
        assert_eq!(julian(-6 * MICROS_PER_HOUR), 2_440_587.75);
        assert_eq!(part("epoch").double_of_days(day).expect("seconds"), 1_577_836_800.0);
        assert_eq!(part("jd").double_of_days(day).expect("days"), 2_458_850.0);
        assert_eq!(part("epoch").double_of_days(-1).expect("seconds"), -86_400.0);
        let far = i64::from(days_from_civil(290_309, 12, 21)) * MICROS_PER_DAY + 86_399_000_000;
        assert_eq!(julian(far), 107_754_599.999_988_42);
    }

    /// A part that is whole is still a double when the specifier was a column, since nothing at
    /// binding time could look at it, and the number has to come out the same either way.
    #[test]
    fn a_whole_part_read_as_a_double_is_the_same_number() {
        let day = days_from_civil(2020, 3, 15);
        assert_eq!(part("year").double_of_days(day).expect("a year"), 2_020.0);
        let noon = i64::from(day) * MICROS_PER_DAY;
        assert_eq!(part("month").double_of_micros(noon).expect("a month"), 3.0);
        assert_eq!(part("month").double_of_interval(14, 0, 0).expect("months"), 2.0);
    }

    /// The seconds a length is worth, where a year is three hundred and sixty five and a quarter
    /// days and a month is thirty, so twelve months and one year are the same number and eleven
    /// months are not eleven twelfths of it.
    #[test]
    fn the_seconds_in_a_length_count_the_years_first() {
        let epoch = |months, days, micros| {
            part("epoch").double_of_interval(months, days, micros).expect("seconds")
        };
        assert_eq!(
            epoch(14, 3, 4 * MICROS_PER_HOUR + 5 * MICROS_PER_MINUTE + 6 * 1_000_000),
            37_015_506.0
        );
        assert_eq!(epoch(12, 0, 0), 31_557_600.0);
        assert_eq!(epoch(1, 0, 0), 2_592_000.0);
        assert_eq!(epoch(13, 0, 0), 34_149_600.0);
        assert_eq!(epoch(-14, 0, 0), -36_741_600.0);
        assert_eq!(epoch(0, 0, 1_500_000), 1.5);
        assert_eq!(epoch(0, 0, -1_500_000), -1.5);
    }

    /// `INTERVAL '1 year 2 months 3 days 04:05:06.7'`, which has something in all three fields.
    const LENGTH: (i32, i32, i64) =
        (14, 3, 4 * MICROS_PER_HOUR + 5 * MICROS_PER_MINUTE + 6_700_000);

    fn of_interval(spelling: &str, (months, days, micros): (i32, i32, i64)) -> i64 {
        part(spelling).of_interval(months, days, micros).expect("a part of an interval")
    }

    /// Every one of these was read off the binary, and the ones worth looking twice at are the day
    /// and the hour of thirty six hours, which is a length that has no days in it at all.
    #[test]
    fn every_part_of_an_interval_reads_one_field_and_leaves_the_others_alone() {
        for (spelling, answer) in [
            ("year", 1),
            ("month", 2),
            ("day", 3),
            ("hour", 4),
            ("minute", 5),
            ("second", 6),
            ("millisecond", 6_700),
            ("microsecond", 6_700_000),
            ("quarter", 1),
        ] {
            assert_eq!(of_interval(spelling, LENGTH), answer, "{spelling} of a length");
        }
        let hours = (0, 0, 36 * MICROS_PER_HOUR);
        assert_eq!(of_interval("day", hours), 0);
        assert_eq!(of_interval("hour", hours), 36);
        assert_eq!(of_interval("minute", (0, 0, 125 * MICROS_PER_MINUTE)), 5);
        assert_eq!(of_interval("second", (0, 0, 3_661 * MICROS_PER_SECOND)), 1);
        assert_eq!(of_interval("millisecond", (0, 0, 3_661 * MICROS_PER_SECOND)), 1_000);
        assert_eq!(of_interval("decade", (300, 0, 0)), 2);
        assert_eq!(of_interval("century", (3_000, 0, 0)), 2);
        assert_eq!(of_interval("millennium", (30_000, 0, 0)), 2);
        assert_eq!(of_interval("quarter", (11, 0, 0)), 4);
        assert_eq!(of_interval("quarter", (0, 0, 0)), 1);
    }

    /// A length below zero divides towards zero rather than downwards, which is the opposite of
    /// what a timestamp before the epoch does, and both were measured rather than picked.
    #[test]
    fn a_length_below_zero_counts_towards_zero() {
        assert_eq!(of_interval("year", (-11, 0, 0)), 0);
        assert_eq!(of_interval("year", (-14, 0, 0)), -1);
        assert_eq!(of_interval("month", (-14, 0, 0)), -2);
        assert_eq!(of_interval("decade", (-1_500, 0, 0)), -12);
        assert_eq!(of_interval("second", (0, 0, -3_661 * MICROS_PER_SECOND)), -1);
        assert_eq!(of_interval("microsecond", (0, 0, -6_700_000)), -6_700_000);
        assert_eq!(of_interval("quarter", (-5, 0, 0)), 0);
        assert_eq!(of_interval("quarter", (-1, 0, 0)), 1);
    }

    fn truncate_interval(
        spelling: &str,
        (months, days, micros): (i32, i32, i64),
    ) -> (i32, i32, i64) {
        part(spelling).truncate_interval(months, days, micros).expect("a truncated interval")
    }

    /// A truncation clears every field below the part and leaves the ones above it, which is why
    /// the months survive a truncation to a day and the days do not survive one to a month.
    #[test]
    fn truncating_an_interval_clears_everything_under_the_part() {
        let clock = 6 * MICROS_PER_HOUR + 7 * MICROS_PER_MINUTE + 8_900_000;
        let length = (14, 10, clock);
        for (spelling, answer) in [
            ("millennium", (0, 0, 0)),
            ("century", (0, 0, 0)),
            ("decade", (0, 0, 0)),
            ("year", (12, 0, 0)),
            ("isoyear", (12, 0, 0)),
            ("quarter", (12, 0, 0)),
            ("month", (14, 0, 0)),
            ("week", (14, 7, 0)),
            ("yearweek", (14, 7, 0)),
            ("day", (14, 10, 0)),
            ("dow", (14, 10, 0)),
            ("doy", (14, 10, 0)),
            ("hour", (14, 10, 6 * MICROS_PER_HOUR)),
            ("minute", (14, 10, 6 * MICROS_PER_HOUR + 7 * MICROS_PER_MINUTE)),
            ("second", (14, 10, clock - 900_000)),
            ("epoch", (14, 10, clock - 900_000)),
            ("millisecond", (14, 10, clock)),
            ("microsecond", (14, 10, clock)),
        ] {
            assert_eq!(truncate_interval(spelling, length), answer, "{spelling} of a length");
        }
    }

    /// A length below zero is cut down towards zero rather than downwards, which is the one place
    /// this and the timestamp truncation above it disagree, since a length has no epoch to be
    /// before.
    #[test]
    fn truncating_a_length_below_zero_cuts_towards_zero() {
        assert_eq!(truncate_interval("decade", (-125, 0, 0)), (-120, 0, 0));
        assert_eq!(truncate_interval("year", (-14, 5, 6 * MICROS_PER_HOUR)), (-12, 0, 0));
        assert_eq!(truncate_interval("week", (0, -20, 0)), (0, -14, 0));
        assert_eq!(truncate_interval("second", (0, 0, -6_987_654)), (0, 0, -6_000_000));
        assert_eq!(truncate_interval("millisecond", (0, 0, -6_987_654)), (0, 0, -6_987_000));
    }

    /// The one part a length cannot be truncated to, and its sentence is the sentence a moment gets
    /// with the last word taken off, which is upstream's and not a typo here.
    #[test]
    fn a_length_truncated_to_an_era_says_so_without_the_word_statistics() {
        let error = part("era").truncate_interval(14, 10, 0).expect_err("an era does not truncate");
        assert_eq!(
            error.to_string(),
            "Not implemented Error: Specifier type not implemented for DATETRUNC"
        );
    }

    /// The parts that need a calendar, refused with the type in the sentence and the specifier
    /// spelled the way it was written rather than the way it was matched.
    #[test]
    fn the_parts_an_interval_does_not_have_name_the_type_and_the_spelling() {
        for spelling in ["week", "dow", "doy", "isoyear", "isodow", "era", "yearweek", "WEEKDAY"] {
            let error = part(spelling).of_an_interval(spelling).expect_err("not an interval part");
            assert_eq!(
                error.to_string(),
                format!("Not implemented Error: \"interval\" units \"{spelling}\" not recognized")
            );
        }
        assert_eq!(part("month").of_an_interval("month").expect("an interval part"), Part::Month);
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
