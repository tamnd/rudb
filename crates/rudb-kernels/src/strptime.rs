//! `strptime` and `try_strptime`, which read a timestamp out of text in a format of the caller's
//! choosing.
//!
//! The format is the one `strftime` writes, taken apart the same way, and the reading follows
//! upstream's `StrpTimeFormat::Parse` step for step: the same widths for the numbers, the same
//! rules for which of a month and day, a week number or a day of the year says what the date is,
//! and the same sentences when the text does not fit, which the corpus compares word for word.
//!
//! The format can also be a list of formats, which are tried in order until one fits. When none
//! does, the pin quotes the first format but gives the reason the last one failed, and so does
//! this.

use rudb_common::{Error, LogicalType, Result, Value, civil_from_days, days_from_civil};

use rudb_vector::{Form, Vector};

use crate::strftime::{Format, MONTHS, Spec, WEEKDAYS};

const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;
const NANOS_PER_DAY: i64 = MICROS_PER_DAY * 1_000;

/// The formats a call reads with, in the order they are tried, each with the text it was written
/// as for the error.
#[derive(Clone, Debug)]
pub struct Formats {
    texts: Vec<String>,
    formats: Vec<Format>,
}

/// A date that is not written with numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Special {
    Infinity,
    NegativeInfinity,
    Epoch,
}

/// What a format read out of a text, before it is a timestamp. The year, month and day start at
/// 1900-01-01 and the time at midnight, which is what a part the format leaves out is.
#[derive(Clone, Copy, Debug)]
struct Parsed {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
    /// The offset from UTC in seconds, which is taken off the time.
    offset: i64,
    special: Option<Special>,
}

/// Why a text did not fit, and where, which the pin marks with a caret under the text.
struct Failure {
    message: String,
    position: Option<usize>,
}

fn failed<T>(message: impl Into<String>, position: usize) -> std::result::Result<T, Failure> {
    Err(Failure { message: message.into(), position: Some(position) })
}

/// The pin's idea of a space, which is C's.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// How many digits a numeric specifier reads at most, or `None` for the ones that are not numbers.
fn width(spec: Spec) -> Option<usize> {
    Some(match spec {
        Spec::Weekday | Spec::IsoWeekday => 1,
        Spec::DayPadded
        | Spec::Day
        | Spec::MonthPadded
        | Spec::Month
        | Spec::ShortYearPadded
        | Spec::ShortYear
        | Spec::HourPadded
        | Spec::Hour
        | Spec::Hour12Padded
        | Spec::Hour12
        | Spec::MinutePadded
        | Spec::Minute
        | Spec::SecondPadded
        | Spec::Second
        | Spec::SundayWeek
        | Spec::MondayWeek
        | Spec::IsoWeek => 2,
        Spec::Millis | Spec::DayOfYearPadded | Spec::DayOfYear => 3,
        Spec::Year | Spec::IsoYear => 4,
        Spec::Micros => 6,
        Spec::Nanos => 9,
        _ => return None,
    })
}

fn month_or_day(spec: Spec) -> bool {
    matches!(spec, Spec::DayPadded | Spec::Day | Spec::MonthPadded | Spec::Month)
}

fn year(spec: Spec) -> bool {
    matches!(spec, Spec::ShortYearPadded | Spec::ShortYear | Spec::Year)
}

/// The Monday on or before a day, since day zero is a Thursday.
fn monday_of(days: i64) -> i64 {
    days - (days + 3).rem_euclid(7)
}

fn day_number(year: i64, month: u32, day: u32) -> i64 {
    i64::from(days_from_civil(i32::try_from(year).unwrap_or(i32::MAX), month, day))
}

/// Which of a list of names starts the text at `pos`, ignoring case, moving `pos` past it.
fn one_of<'a>(data: &[u8], pos: &mut usize, names: impl Iterator<Item = &'a str>) -> Option<usize> {
    for (at, name) in names.enumerate() {
        let name = name.as_bytes();
        let Some(text) = data.get(*pos..*pos + name.len()) else { continue };
        if text.eq_ignore_ascii_case(name) {
            *pos += name.len();
            return Some(at);
        }
    }
    None
}

/// Upstream's `Timestamp::TryParseUTCOffset` in its lenient form, which is `±H`, `±HH`, then
/// optionally `MM` or `:MM`, then `:SS` if there were colons. Anything too short to start one is
/// read as no offset at all rather than refused.
fn utc_offset(data: &[u8], pos: &mut usize) -> Option<i64> {
    let at = |index: usize| data.get(index).copied().unwrap_or(0);
    let len = data.len();
    let mut cur = *pos;
    if cur + 2 > len {
        return Some(0);
    }
    let sign = at(cur);
    if sign != b'+' && sign != b'-' {
        return None;
    }
    let signed = |value: i64| if sign == b'-' { -value } else { value };
    cur += 1;
    if !at(cur).is_ascii_digit() {
        return None;
    }
    let mut hours = i64::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        hours = hours * 10 + i64::from(at(cur) - b'0');
        cur += 1;
    }
    let hours = signed(hours);
    if cur >= len {
        *pos = cur;
        return Some(hours * 3_600);
    }
    let colons = at(cur) == b':';
    if colons {
        cur += 1;
    }
    if cur + 1 > len || !at(cur).is_ascii_digit() {
        *pos = cur;
        return (!colons).then_some(hours * 3_600);
    }
    let mut minutes = i64::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        minutes = minutes * 10 + i64::from(at(cur) - b'0');
        cur += 1;
    }
    let minutes = signed(minutes);
    if cur >= len || !colons || at(cur) != b':' {
        *pos = cur;
        return Some(hours * 3_600 + minutes * 60);
    }
    cur += 1;
    if cur + 1 > len || !at(cur).is_ascii_digit() {
        *pos = cur;
        return None;
    }
    let mut seconds = i64::from(at(cur) - b'0');
    cur += 1;
    if at(cur).is_ascii_digit() {
        seconds = seconds * 10 + i64::from(at(cur) - b'0');
        cur += 1;
    }
    *pos = cur;
    Some(hours * 3_600 + minutes * 60 + signed(seconds))
}

/// The one special the text is, if it starts with one, and where it ends.
fn special(data: &[u8]) -> Option<(Special, usize)> {
    if data.len() <= 4 {
        return None;
    }
    let words: &[(&str, Special)] = if data[0].is_ascii_alphabetic() {
        &[("infinity", Special::Infinity), ("epoch", Special::Epoch)]
    } else if data[0] == b'-' {
        &[("-infinity", Special::NegativeInfinity)]
    } else {
        return None;
    };
    words.iter().find_map(|(word, special)| {
        let word = word.as_bytes();
        let text = data.get(..word.len())?;
        text.eq_ignore_ascii_case(word).then_some((*special, word.len()))
    })
}

/// A string read as a date with one format, which is how `read_json` tries its date formats.
pub(crate) fn try_date(format: &Format, text: &str) -> Option<i32> {
    i32::try_from(parse(format, text).ok()?.days()?).ok()
}

/// A string read as a timestamp with one format, which is how `read_json` tries its timestamp
/// formats.
pub(crate) fn try_timestamp(format: &Format, text: &str) -> Option<i64> {
    parse(format, text).ok()?.micros(false).ok().flatten()
}

/// Upstream's `StrpTimeFormat::Parse`.
#[allow(clippy::too_many_lines)]
fn parse(format: &Format, text: &str) -> std::result::Result<Parsed, Failure> {
    let mut parsed = Parsed {
        year: 1900,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        nanos: 0,
        offset: 0,
        special: None,
    };
    // The leading spaces are skipped, and every position after this counts from past them.
    let data = text.as_bytes();
    let data = &data[data.iter().take_while(|byte| is_space(**byte)).count()..];
    let size = data.len();
    let at = |index: usize| data.get(index).copied().unwrap_or(0);
    if let Some((special, mut pos)) = special(data) {
        while pos < size && is_space(data[pos]) {
            pos += 1;
        }
        if pos != size {
            return failed("Special timestamp did not match: trailing characters", pos);
        }
        parsed.special = Some(special);
        return Ok(parsed);
    }
    let mut pos = 0;
    let mut ampm = None;
    // `Spec::Weekday` stands for no offset specifier yet, as `WEEKDAY_DECIMAL` does upstream.
    let mut offset_spec = Spec::Weekday;
    let (mut weekno, mut weekday, mut yearday) = (0_i64, 0_i64, 0_i64);
    let mut has_weekday = false;
    // Out of range to tell a second one from the first.
    let (mut iso_year, mut iso_week, mut iso_weekday) = (10_000_i64, 54_i64, 8_i64);
    for index in 0.. {
        let literal = format.literals[index].as_bytes();
        let mut l = 0;
        while l < literal.len() {
            // A run of spaces in the format matches a run of spaces in the text.
            if is_space(literal[l]) {
                if !is_space(at(pos)) {
                    return failed(
                        format!("Space does not match, expected {}", format.literals[index]),
                        pos,
                    );
                }
                pos += 1;
                while pos < size && is_space(data[pos]) {
                    pos += 1;
                }
                l += 1;
                while l < literal.len() && is_space(literal[l]) {
                    l += 1;
                }
                continue;
            }
            let matched = at(pos) == literal[l];
            pos += 1;
            l += 1;
            if !matched {
                return failed(
                    format!("Literal does not match, expected {}", format.literals[index]),
                    pos,
                );
            }
        }
        let Some(&spec) = format.specs.get(index) else { break };
        if let Some(width) = width(spec) {
            let start = pos;
            let mut number = 0_i64;
            let mut digits = 0;
            while pos < size && pos < start + width && data[pos].is_ascii_digit() {
                number = number * 10 + i64::from(data[pos] - b'0');
                pos += 1;
                digits += 1;
            }
            if pos == start {
                return failed("Expected a number", start);
            }
            let out_of_range = |what: &str| failed(what.to_string(), start);
            match spec {
                Spec::DayPadded | Spec::Day => {
                    if !(1..=31).contains(&number) {
                        return out_of_range("Day out of range, expected a value between 1 and 31");
                    }
                    parsed.day = number;
                    offset_spec = spec;
                }
                Spec::MonthPadded | Spec::Month => {
                    if !(1..=12).contains(&number) {
                        return out_of_range(
                            "Month out of range, expected a value between 1 and 12",
                        );
                    }
                    parsed.month = number;
                    offset_spec = spec;
                }
                Spec::ShortYearPadded | Spec::ShortYear | Spec::Year => {
                    if matches!(offset_spec, Spec::IsoYear | Spec::IsoWeek | Spec::Weekday) {
                        offset_spec = spec;
                    }
                    parsed.year = match spec {
                        Spec::Year => number,
                        // Python's crossover, where 69 and up are the 1900s.
                        _ if number >= 69 => 1900 + number,
                        _ => 2000 + number,
                    };
                }
                Spec::IsoYear => {
                    match offset_spec {
                        now if month_or_day(now) || year(now) => {}
                        Spec::Weekday => offset_spec = spec,
                        Spec::IsoYear | Spec::IsoWeek => {
                            if iso_year <= 9999 {
                                return out_of_range("Multiple ISO year offsets specified");
                            }
                        }
                        _ => return out_of_range("Incompatible ISO year offset specified"),
                    }
                    iso_year = number;
                }
                Spec::HourPadded | Spec::Hour => {
                    if number >= 24 {
                        return out_of_range(
                            "Hour out of range, expected a value between 0 and 23",
                        );
                    }
                    parsed.hour = number;
                }
                Spec::Hour12Padded | Spec::Hour12 => {
                    if !(1..=12).contains(&number) {
                        return out_of_range(
                            "Hour12 out of range, expected a value between 1 and 12",
                        );
                    }
                    parsed.hour = number;
                }
                Spec::MinutePadded | Spec::Minute => {
                    if number >= 60 {
                        return out_of_range(
                            "Minutes out of range, expected a value between 0 and 59",
                        );
                    }
                    parsed.minute = number;
                }
                Spec::SecondPadded | Spec::Second => {
                    if number >= 60 {
                        return out_of_range(
                            "Seconds out of range, expected a value between 0 and 59",
                        );
                    }
                    parsed.second = number;
                }
                // A fraction shorter than its width is padded on the right, so `.5` is half.
                Spec::Nanos | Spec::Micros | Spec::Millis => {
                    for _ in digits..width {
                        number *= 10;
                    }
                    parsed.nanos = match spec {
                        Spec::Millis => number * 1_000_000,
                        Spec::Micros => number * 1_000,
                        _ => number,
                    };
                }
                Spec::SundayWeek | Spec::MondayWeek => {
                    match offset_spec {
                        now if month_or_day(now) => {}
                        now if year(now) || now == Spec::Weekday => offset_spec = spec,
                        _ => return out_of_range("Multiple week offsets specified"),
                    }
                    if number > 53 {
                        return out_of_range(
                            "Week out of range, expected a value between 0 and 53",
                        );
                    }
                    weekno = number;
                }
                Spec::Weekday => {
                    if number > 6 {
                        return out_of_range(
                            "Weekday out of range, expected a value between 0 and 6",
                        );
                    }
                    has_weekday = true;
                    weekday = number;
                }
                Spec::IsoWeek => {
                    match offset_spec {
                        now if year(now) => {
                            return out_of_range(
                                "ISO week offsets are incompatible with non-ISO year specifiers. \
                                 Use '%G' instead",
                            );
                        }
                        now if month_or_day(now) => {}
                        Spec::Weekday => offset_spec = spec,
                        Spec::IsoWeek | Spec::IsoYear => {
                            if iso_week <= 53 {
                                return out_of_range("Multiple ISO week offsets specified");
                            }
                        }
                        _ => return out_of_range("Incompatible ISO week offset specified"),
                    }
                    if !(1..=53).contains(&number) {
                        return out_of_range(
                            "ISO week offset out of range, expected a value between 1 and 53",
                        );
                    }
                    iso_week = number;
                }
                Spec::IsoWeekday => {
                    if iso_weekday <= 7 {
                        return out_of_range("Multiple ISO weekday offsets specified");
                    }
                    if !(1..=7).contains(&number) {
                        return out_of_range(
                            "ISO weekday offset out of range, expected a value between 1 and 7",
                        );
                    }
                    iso_weekday = number;
                }
                Spec::DayOfYearPadded | Spec::DayOfYear => {
                    match offset_spec {
                        now if month_or_day(now) => {}
                        now if year(now) || now == Spec::Weekday => offset_spec = spec,
                        _ => return out_of_range("Multiple year offsets specified"),
                    }
                    if !(1..=366).contains(&number) {
                        return out_of_range(
                            "Year day out of range, expected a value between 1 and 366",
                        );
                    }
                    yearday = number;
                }
                _ => {}
            }
            continue;
        }
        match spec {
            Spec::AmPm => {
                if pos + 2 > size {
                    return failed("Expected AM/PM", pos);
                }
                if !data[pos + 1].eq_ignore_ascii_case(&b'm') {
                    return failed("Expected AM/PM", pos);
                }
                ampm = Some(match data[pos].to_ascii_lowercase() {
                    b'p' => true,
                    b'a' => false,
                    _ => return failed("Expected AM/PM", pos),
                });
                pos += 2;
            }
            // The day names are read but say nothing the date does not.
            Spec::WeekdayShort => {
                if one_of(data, &mut pos, WEEKDAYS.iter().map(|name| &name[..3])).is_none() {
                    return failed(
                        "Expected an abbreviated day name (Mon, Tue, Wed, Thu, Fri, Sat, Sun)",
                        pos,
                    );
                }
            }
            Spec::WeekdayLong => {
                if one_of(data, &mut pos, WEEKDAYS.iter().copied()).is_none() {
                    return failed("Expected a full day name (Monday, Tuesday, etc...)", pos);
                }
            }
            Spec::MonthShort => {
                let Some(month) = one_of(data, &mut pos, MONTHS.iter().map(|name| &name[..3]))
                else {
                    return failed(
                        "Expected an abbreviated month name (Jan, Feb, Mar, etc..)",
                        pos,
                    );
                };
                parsed.month = i64::try_from(month).unwrap_or(0) + 1;
            }
            Spec::MonthLong => {
                let Some(month) = one_of(data, &mut pos, MONTHS.iter().copied()) else {
                    return failed("Expected a full month name (January, February, etc...)", pos);
                };
                parsed.month = i64::try_from(month).unwrap_or(0) + 1;
            }
            Spec::Offset => {
                let Some(offset) = utc_offset(data, &mut pos) else {
                    return failed("Expected ±HH[MM] or -HH[:MM[:SS]]", pos);
                };
                parsed.offset = offset;
            }
            // The name of a zone is read and, without the ICU extension, not used.
            Spec::ZoneName => {
                while pos < size && is_space(data[pos]) {
                    pos += 1;
                }
                let begin = pos;
                while pos < size
                    && (data[pos].is_ascii_alphanumeric() || b"_/+-:".contains(&data[pos]))
                {
                    pos += 1;
                }
                if pos == begin {
                    return failed("Empty Time Zone name", begin);
                }
            }
            _ => {}
        }
    }
    while pos < size && is_space(data[pos]) {
        pos += 1;
    }
    if pos != size {
        return failed("Full specifier did not match: trailing characters", pos);
    }
    if let Some(pm) = ampm {
        if parsed.hour > 12 {
            return Err(Failure {
                message: format!(
                    "Invalid hour: {} AM/PM, expected an hour within the range [0..12]",
                    parsed.hour
                ),
                position: None,
            });
        }
        if pm && parsed.hour != 12 {
            parsed.hour += 12;
        } else if !pm && parsed.hour == 12 {
            parsed.hour = 0;
        }
    }
    let set_date = |parsed: &mut Parsed, days: i64| {
        let (year, month, day) = civil_from_days(i32::try_from(days).unwrap_or(i32::MAX));
        parsed.year = i64::from(year);
        parsed.month = i64::from(month);
        parsed.day = i64::from(day);
    };
    match offset_spec {
        Spec::IsoYear | Spec::IsoWeek => {
            let iso_year = if iso_year > 9999 { 1900 } else { iso_year };
            let iso_week = if iso_week > 53 { 1 } else { iso_week };
            let iso_weekday = if iso_weekday > 7 { 1 } else { iso_weekday };
            // The Gregorian and the ISO year agree on the year of January 4.
            let week1 = monday_of(day_number(iso_year, 1, 4));
            set_date(&mut parsed, week1 + (iso_week - 1) * 7 + (iso_weekday - 1));
        }
        Spec::SundayWeek | Spec::MondayWeek => {
            if has_weekday {
                weekday = (weekday + 7 - i64::from(offset_spec == Spec::MondayWeek)) % 7;
            }
            let jan1 = day_number(parsed.year, 1, 1);
            let mut start = monday_of(jan1) - i64::from(offset_spec == Spec::SundayWeek);
            if start >= jan1 {
                start -= 7;
            }
            set_date(&mut parsed, start + weekno * 7 + weekday);
        }
        Spec::DayOfYearPadded | Spec::DayOfYear => {
            let jan1 = day_number(parsed.year, 1, 1);
            set_date(&mut parsed, jan1 + yearday - 1);
        }
        _ => {}
    }
    Ok(parsed)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Parsed {
    /// The day number, or `None` for a day the month does not have.
    fn days(&self) -> Option<i64> {
        let month = u32::try_from(self.month).ok()?;
        let day = u32::try_from(self.day).ok()?;
        (self.day <= days_in_month(self.year, self.month)
            && i32::try_from(self.year).is_ok_and(|year| (-290_307..=294_247).contains(&year)))
        .then(|| day_number(self.year, month, day))
    }

    fn date_error(&self) -> Error {
        Error::conversion(format!("Date out of range: {}-{}-{}", self.year, self.month, self.day))
    }

    /// The seconds of the day less the offset, which can go below zero or past a day.
    fn seconds(&self) -> i64 {
        self.hour * 3_600 + self.minute * 60 + self.second - self.offset
    }

    /// The timestamp in microseconds, with the nanoseconds rounded, or `None` where `try_strptime`
    /// answers a null. A special reads as its timestamp only when `strict`, since upstream's try
    /// path never looks at it and reads the date it started from.
    fn micros(&self, strict: bool) -> std::result::Result<Option<i64>, Error> {
        match self.special {
            Some(Special::Infinity) if strict => return Ok(Some(i64::MAX)),
            Some(Special::NegativeInfinity) if strict => return Ok(Some(-i64::MAX)),
            Some(Special::Epoch) if strict => return Ok(Some(0)),
            _ => {}
        }
        let Some(days) = self.days() else {
            return if strict { Err(self.date_error()) } else { Ok(None) };
        };
        let micros = days
            .checked_mul(MICROS_PER_DAY)
            .and_then(|stamp| stamp.checked_add(self.seconds() * 1_000_000))
            .and_then(|stamp| stamp.checked_add((self.nanos + 500) / 1_000));
        match micros {
            Some(stamp) if !crate::datetime::infinite_stamp(stamp) => Ok(Some(stamp)),
            _ if strict => Err(Error::conversion("Date and time not in timestamp range")),
            _ => Ok(None),
        }
    }

    /// The timestamp in nanoseconds, which keeps the nanoseconds as they were read.
    fn nanos(&self, strict: bool) -> std::result::Result<Option<i64>, Error> {
        match self.special {
            Some(Special::Infinity) if strict => return Ok(Some(i64::MAX)),
            Some(Special::NegativeInfinity) if strict => return Ok(Some(-i64::MAX)),
            Some(Special::Epoch) if strict => return Ok(Some(0)),
            _ => {}
        }
        let Some(days) = self.days() else {
            return if strict { Err(self.date_error()) } else { Ok(None) };
        };
        let Some(stamp) = days.checked_mul(NANOS_PER_DAY) else {
            return if strict {
                Err(Error::conversion(format!(
                    "Date out of nanosecond range: {}-{}-{}",
                    self.year, self.month, self.day
                )))
            } else {
                Ok(None)
            };
        };
        match stamp.checked_add(self.seconds() * 1_000_000_000 + self.nanos) {
            Some(stamp) if !strict && crate::datetime::infinite_stamp(stamp) => Ok(None),
            Some(stamp) => Ok(Some(stamp)),
            None if strict => {
                Err(Error::conversion("Overflow exception in date/time -> timestamp_ns conversion"))
            }
            None => Ok(None),
        }
    }
}

impl Formats {
    /// Takes apart the format or the list of formats a call was given, or `None` for a null one,
    /// which makes every row null.
    ///
    /// # Errors
    ///
    /// The pin's invalid input errors for a format that does not parse, an empty list and a format
    /// that is not text.
    pub fn from_value(format: &Value) -> Result<Option<Self>> {
        let texts: Vec<String> = match format {
            Value::Null => return Ok(None),
            Value::Varchar(format) => vec![format.clone()],
            Value::List { element: LogicalType::Varchar, values } => {
                if values.is_empty() {
                    return Err(Error::invalid_input("strptime format list must not be empty"));
                }
                values.iter().map(ToString::to_string).collect()
            }
            _ => return Err(Error::invalid_input("strptime format must be a string")),
        };
        let formats = texts.iter().map(|text| Format::parse(text)).collect::<Result<_>>()?;
        Ok(Some(Self { texts, formats }))
    }

    /// The type a call answers: a nanosecond timestamp if any format reads `%n`, a timestamp with
    /// a time zone if any reads `%z`, and a plain timestamp otherwise. rudb has no nanosecond
    /// timestamp with a time zone, so a format with both answers the nanosecond one, in UTC.
    #[must_use]
    pub fn returns(&self) -> LogicalType {
        if self.formats.iter().any(Format::has_nanos) {
            LogicalType::TimestampNs
        } else if self.formats.iter().any(Format::has_offset) {
            LogicalType::TimestampTz
        } else {
            LogicalType::Timestamp
        }
    }

    /// Reads one text, as `strptime` when `strict` and as `try_strptime` when not.
    ///
    /// # Errors
    ///
    /// When `strict`, the pin's error for a text no format fits, and a conversion error for a date
    /// the month does not have or a timestamp out of range.
    pub fn read(&self, text: &Value, returns: &LogicalType, strict: bool) -> Result<Value> {
        let Value::Varchar(text) = text else {
            return Ok(Value::Null);
        };
        let mut last = None;
        for format in &self.formats {
            match parse(format, text) {
                Ok(parsed) => {
                    let stamp = if *returns == LogicalType::TimestampNs {
                        parsed.nanos(strict)?
                    } else {
                        parsed.micros(strict)?
                    };
                    return Ok(stamp.map_or(Value::Null, |stamp| match returns {
                        LogicalType::TimestampNs => Value::TimestampNs(stamp),
                        LogicalType::TimestampTz => Value::TimestampTz(stamp),
                        _ => Value::Timestamp(stamp),
                    }));
                }
                Err(failure) => last = Some(failure),
            }
        }
        let Some(failure) = last.filter(|_| strict) else {
            return Ok(Value::Null);
        };
        let caret = failure
            .position
            .map(|position| format!("{text}\n{}^", " ".repeat(position)))
            .unwrap_or_default();
        Err(Error::invalid_input(format!(
            "Could not parse string \"{text}\" according to format specifier \"{}\"\n{caret}\n\
             Error: {}",
            self.texts[0], failure.message
        )))
    }
}

/// `strptime` or `try_strptime` on one row.
///
/// # Errors
///
/// If the format does not parse, or, for `strptime`, if the text does not fit it.
pub(crate) fn value(
    text: &Value,
    format: &Value,
    returns: &LogicalType,
    strict: bool,
) -> Result<Value> {
    match Formats::from_value(format)? {
        Some(formats) => formats.read(text, returns, strict),
        None => Ok(Value::Null),
    }
}

/// `strptime` or `try_strptime` over a batch whose format is the same on every row, which is every
/// one the binder lets through. The formats are taken apart once for the batch.
///
/// # Errors
///
/// As [`value`].
pub(crate) fn vectorized(
    args: &[&Vector],
    returns: &LogicalType,
    strict: bool,
    rows: usize,
) -> Result<Option<Vector>> {
    let [text, format] = args else {
        return Ok(None);
    };
    if format.form() != Form::Constant {
        return Ok(None);
    }
    let Some(formats) = Formats::from_value(&format.try_value_at(0)?)? else {
        return Ok(Some(Vector::constant(returns.clone(), Value::Null, rows)));
    };
    let read: Vec<Value> = (0..rows)
        .map(|row| formats.read(&text.value_at(row), returns, strict))
        .collect::<Result<_>>()?;
    Vector::from_values(returns.clone(), &read).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, format: &str) -> String {
        let formats = Formats::from_value(&Value::Varchar(format.into())).unwrap().unwrap();
        let returns = formats.returns();
        match formats.read(&Value::Varchar(text.into()), &returns, true) {
            Ok(value) => value.to_string(),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn a_text_is_read_the_way_the_pin_reads_it() {
        assert_eq!(read("2020-01-05 10:11:12", "%Y-%m-%d %H:%M:%S"), "2020-01-05 10:11:12");
        assert_eq!(read("  2020-1-5", "%Y-%m-%d"), "2020-01-05 00:00:00");
        assert_eq!(read("20 45", "%y %j"), "2020-02-14 00:00:00");
        assert_eq!(read("2020 10 3", "%Y %U %w"), "2020-03-11 00:00:00");
        assert_eq!(read("2020 10 3", "%Y %W %w"), "2020-03-11 00:00:00");
        assert_eq!(read("2020 10 3", "%G %V %u"), "2020-03-04 00:00:00");
        assert_eq!(read("10:11:12.5", "%H:%M:%S.%g"), "1900-01-01 10:11:12.5");
    }

    #[test]
    fn a_text_that_does_not_fit_is_marked_where_it_stops_fitting() {
        let said = read("12345-1-5", "%Y-%m-%d");
        assert!(
            said.contains("12345-1-5\n     ^\nError: Literal does not match, expected -"),
            "{said}"
        );
        let said = read("13 PM", "%H %p");
        assert!(said.contains("\"%H %p\"\n\nError: Invalid hour: 13 AM/PM"), "{said}");
    }
}
