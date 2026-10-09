//! `to_char`, `to_timestamp` and `to_date` for the date and time types.
//!
//! This is a port of the `DCH` part of `src/backend/utils/adt/formatting.c`: `parse_format`,
//! `DCH_to_char`, `DCH_from_char` and `do_to_timestamp`, with the ISO week functions of
//! `timestamp.c`. The structure follows the C code, so that a difference against the server can
//! be found by reading the two side by side.
//!
//! A [`DateTemplate`] is the parsed template. The caller keeps it while the template text does
//! not change, as the cache of the server does. The `TM` prefix gives the names of the C locale,
//! which are the English names.

use std::fmt::Write;

use rudb_common::SqlState;

use super::decode::{
    Abbrev, DATE_M, DAY, DateTimeInput, DtErr, MONTH, Tm, YEAR, ZoneRef, determine_abbrev_offset,
    determine_offset, is_leap, is_valid_julian, m, set_date, strtol, tm2timestamp, validate_date,
};
use super::{
    AbbrevMeaning, DATE_END_JULIAN, DAYS, Fields, Interval, MONTHS, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE,
    UNIX_TO_POSTGRES_USECS, USECS_PER_SEC, adjust_timestamp, date2j, j2date, out_of_range,
    split_time,
};
use crate::error::TypeError;
use crate::number::is_space;

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS_FULL: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const ROMAN_UPPER: [&str; 12] =
    ["XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I"];
const ROMAN_LOWER: [&str; 12] =
    ["xii", "xi", "x", "ix", "viii", "vii", "vi", "v", "iv", "iii", "ii", "i"];
const AMPM: [&str; 4] = ["am", "pm", "AM", "PM"];
const AMPM_LONG: [&str; 4] = ["a.m.", "p.m.", "A.M.", "P.M."];
const ADBC: [&str; 4] = ["ad", "bc", "AD", "BC"];
const ADBC_LONG: [&str; 4] = ["a.d.", "b.c.", "A.D.", "B.C."];

/// `DCH_MAX_ITEM_SIZ`: the longest output of one field, for each byte of its keyword.
const MAX_ITEM_SIZE: usize = 12;
/// `TOKMAXLEN`: the longest zone abbreviation that the input reads.
const TOKMAXLEN: usize = 10;
/// `MAX_TZDISP_HOUR`.
const MAX_TZDISP_HOUR: i32 = 15;

// The suffixes of a keyword, the `DCH_SUFFIX` flags.
const FM: u8 = 0x01;
const TH_UPPER: u8 = 0x02;
const TH_LOWER: u8 = 0x04;
const SP: u8 = 0x08;
const TM: u8 = 0x10;

/// The date convention of a keyword, `FromCharDateMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    None,
    Gregorian,
    IsoWeek,
}

/// What a keyword formats. The keywords that differ only in the case of their output, such as
/// `MONTH`, `Month` and `month`, have one value here and the case of the keyword name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Id {
    Meridian { dots: bool },
    Era { dots: bool },
    Century,
    DayName { short: bool },
    DayOfYear { iso: bool },
    DayOfMonth,
    DayOfWeek { iso: bool },
    Fraction(u8),
    Fx,
    Hour24,
    Hour12,
    Week { iso: bool },
    Year { width: u8, iso: bool },
    YearComma,
    Julian,
    Minute,
    Month,
    MonthName { short: bool },
    Millisecond,
    Offset,
    Quarter,
    Roman,
    SecondOfDay,
    Second,
    ZoneHour,
    ZoneMinute,
    Zone,
    Microsecond,
    WeekOfMonth,
}

/// One entry of `DCH_keywords`.
#[derive(Debug)]
struct Key {
    name: &'static str,
    id: Id,
    /// Whether the input of the field is a number, `is_digit`.
    digit: bool,
    mode: Mode,
}

const fn key(name: &'static str, id: Id, digit: bool, mode: Mode) -> Key {
    Key { name, id, digit, mode }
}

use Mode::{Gregorian as G, IsoWeek as I, None as N};

/// `DCH_keywords`, in the order of the C table. The search takes the first keyword that the
/// template starts with, so a longer keyword comes before its prefix.
static KEYS: &[Key] = &[
    key("A.D.", Id::Era { dots: true }, false, N),
    key("A.M.", Id::Meridian { dots: true }, false, N),
    key("AD", Id::Era { dots: false }, false, N),
    key("AM", Id::Meridian { dots: false }, false, N),
    key("B.C.", Id::Era { dots: true }, false, N),
    key("BC", Id::Era { dots: false }, false, N),
    key("CC", Id::Century, true, N),
    key("DAY", Id::DayName { short: false }, false, N),
    key("DDD", Id::DayOfYear { iso: false }, true, G),
    key("DD", Id::DayOfMonth, true, G),
    key("DY", Id::DayName { short: true }, false, N),
    key("Day", Id::DayName { short: false }, false, N),
    key("Dy", Id::DayName { short: true }, false, N),
    key("D", Id::DayOfWeek { iso: false }, true, G),
    key("FF1", Id::Fraction(1), true, N),
    key("FF2", Id::Fraction(2), true, N),
    key("FF3", Id::Fraction(3), true, N),
    key("FF4", Id::Fraction(4), true, N),
    key("FF5", Id::Fraction(5), true, N),
    key("FF6", Id::Fraction(6), true, N),
    key("FX", Id::Fx, false, N),
    key("HH24", Id::Hour24, true, N),
    key("HH12", Id::Hour12, true, N),
    key("HH", Id::Hour12, true, N),
    key("IDDD", Id::DayOfYear { iso: true }, true, I),
    key("ID", Id::DayOfWeek { iso: true }, true, I),
    key("IW", Id::Week { iso: true }, true, I),
    key("IYYY", Id::Year { width: 4, iso: true }, true, I),
    key("IYY", Id::Year { width: 3, iso: true }, true, I),
    key("IY", Id::Year { width: 2, iso: true }, true, I),
    key("I", Id::Year { width: 1, iso: true }, true, I),
    key("J", Id::Julian, true, N),
    key("MI", Id::Minute, true, N),
    key("MM", Id::Month, true, G),
    key("MONTH", Id::MonthName { short: false }, false, G),
    key("MON", Id::MonthName { short: true }, false, G),
    key("MS", Id::Millisecond, true, N),
    key("Month", Id::MonthName { short: false }, false, G),
    key("Mon", Id::MonthName { short: true }, false, G),
    key("OF", Id::Offset, false, N),
    key("P.M.", Id::Meridian { dots: true }, false, N),
    key("PM", Id::Meridian { dots: false }, false, N),
    key("Q", Id::Quarter, true, N),
    key("RM", Id::Roman, false, G),
    key("SSSSS", Id::SecondOfDay, true, N),
    key("SSSS", Id::SecondOfDay, true, N),
    key("SS", Id::Second, true, N),
    key("TZH", Id::ZoneHour, false, N),
    key("TZM", Id::ZoneMinute, true, N),
    key("TZ", Id::Zone, false, N),
    key("US", Id::Microsecond, true, N),
    key("WW", Id::Week { iso: false }, true, G),
    key("W", Id::WeekOfMonth, true, G),
    key("Y,YYY", Id::YearComma, true, G),
    key("YYYY", Id::Year { width: 4, iso: false }, true, G),
    key("YYY", Id::Year { width: 3, iso: false }, true, G),
    key("YY", Id::Year { width: 2, iso: false }, true, G),
    key("Y", Id::Year { width: 1, iso: false }, true, G),
    key("a.d.", Id::Era { dots: true }, false, N),
    key("a.m.", Id::Meridian { dots: true }, false, N),
    key("ad", Id::Era { dots: false }, false, N),
    key("am", Id::Meridian { dots: false }, false, N),
    key("b.c.", Id::Era { dots: true }, false, N),
    key("bc", Id::Era { dots: false }, false, N),
    key("cc", Id::Century, true, N),
    key("day", Id::DayName { short: false }, false, N),
    key("ddd", Id::DayOfYear { iso: false }, true, G),
    key("dd", Id::DayOfMonth, true, G),
    key("dy", Id::DayName { short: true }, false, N),
    key("d", Id::DayOfWeek { iso: false }, true, G),
    key("ff1", Id::Fraction(1), true, N),
    key("ff2", Id::Fraction(2), true, N),
    key("ff3", Id::Fraction(3), true, N),
    key("ff4", Id::Fraction(4), true, N),
    key("ff5", Id::Fraction(5), true, N),
    key("ff6", Id::Fraction(6), true, N),
    key("fx", Id::Fx, false, N),
    key("hh24", Id::Hour24, true, N),
    key("hh12", Id::Hour12, true, N),
    key("hh", Id::Hour12, true, N),
    key("iddd", Id::DayOfYear { iso: true }, true, I),
    key("id", Id::DayOfWeek { iso: true }, true, I),
    key("iw", Id::Week { iso: true }, true, I),
    key("iyyy", Id::Year { width: 4, iso: true }, true, I),
    key("iyy", Id::Year { width: 3, iso: true }, true, I),
    key("iy", Id::Year { width: 2, iso: true }, true, I),
    key("i", Id::Year { width: 1, iso: true }, true, I),
    key("j", Id::Julian, true, N),
    key("mi", Id::Minute, true, N),
    key("mm", Id::Month, true, G),
    key("month", Id::MonthName { short: false }, false, G),
    key("mon", Id::MonthName { short: true }, false, G),
    key("ms", Id::Millisecond, true, N),
    key("of", Id::Offset, false, N),
    key("p.m.", Id::Meridian { dots: true }, false, N),
    key("pm", Id::Meridian { dots: false }, false, N),
    key("q", Id::Quarter, true, N),
    key("rm", Id::Roman, false, G),
    key("sssss", Id::SecondOfDay, true, N),
    key("ssss", Id::SecondOfDay, true, N),
    key("ss", Id::Second, true, N),
    key("tzh", Id::ZoneHour, false, N),
    key("tzm", Id::ZoneMinute, true, N),
    key("tz", Id::Zone, false, N),
    key("us", Id::Microsecond, true, N),
    key("ww", Id::Week { iso: false }, true, G),
    key("w", Id::WeekOfMonth, true, G),
    key("y,yyy", Id::YearComma, true, G),
    key("yyyy", Id::Year { width: 4, iso: false }, true, G),
    key("yyy", Id::Year { width: 3, iso: false }, true, G),
    key("yy", Id::Year { width: 2, iso: false }, true, G),
    key("y", Id::Year { width: 1, iso: false }, true, G),
];

/// The case of the text that a keyword writes, from the case of its name: `MONTH`, `Month` or
/// `month`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Upper,
    Initcap,
    Lower,
}

impl Case {
    fn of(name: &str) -> Case {
        let mut letters = name.bytes().filter(u8::is_ascii_alphabetic);
        match letters.next() {
            Some(first) if first.is_ascii_lowercase() => Case::Lower,
            _ if letters.any(|b| b.is_ascii_lowercase()) => Case::Initcap,
            _ => Case::Upper,
        }
    }

    /// The text of a name from the C tables, which are in initcap or in lower case.
    fn apply(self, name: &str, out: &mut String) {
        match self {
            Case::Upper => out.extend(name.chars().map(|c| c.to_ascii_uppercase())),
            Case::Initcap => out.push_str(name),
            Case::Lower => out.extend(name.chars().map(|c| c.to_ascii_lowercase())),
        }
    }
}

/// One `FormatNode`.
#[derive(Debug, Clone)]
enum Node {
    Action {
        key: &'static Key,
        suffix: u8,
        case: Case,
    },
    /// A space outside quotes.
    Space(char),
    /// An ASCII character that is not a letter, a digit or a space, outside quotes.
    Separator(char),
    /// Any other character, and each character in quotes.
    Char(char),
}

/// A parsed `to_char`, `to_timestamp` or `to_date` template.
#[derive(Debug, Clone)]
pub struct DateTemplate {
    nodes: Vec<Node>,
}

/// `is_separator_char`: an ASCII printable character that is not a letter or a digit.
fn is_separator_char(c: char) -> bool {
    c.is_ascii_graphic() && !c.is_ascii_alphanumeric()
}

impl DateTemplate {
    /// `parse_format` with the `DCH` keywords and suffixes.
    pub fn parse(template: &str) -> DateTemplate {
        let s = template.as_bytes();
        let mut nodes = Vec::with_capacity(template.len());
        let mut i = 0;
        while i < s.len() {
            let mut suffix = 0;
            // A prefix is read even when no keyword follows it, and then it is lost.
            let prefix = match &s[i..] {
                [b'F', b'M', ..] | [b'f', b'm', ..] => FM,
                [b'T', b'M', ..] | [b't', b'm', ..] => TM,
                _ => 0,
            };
            if prefix != 0 {
                suffix |= prefix;
                i += 2;
            }
            if let Some(key) = KEYS.iter().find(|key| s[i..].starts_with(key.name.as_bytes())) {
                i += key.name.len();
                let postfix = match &s[i..] {
                    [b'T', b'H', ..] => TH_UPPER,
                    [b't', b'h', ..] => TH_LOWER,
                    [b'S', b'P', ..] => SP,
                    _ => 0,
                };
                if postfix != 0 {
                    suffix |= postfix;
                    i += 2;
                }
                nodes.push(Node::Action { key, suffix, case: Case::of(key.name) });
                continue;
            }
            let Some(c) = template[i..].chars().next() else { break };
            if c == '"' {
                i += 1;
                while let Some(mut c) = template[i..].chars().next() {
                    if c == '"' {
                        i += 1;
                        break;
                    }
                    // A backslash quotes the next character, if there is one.
                    if c == '\\' && i + 1 < s.len() {
                        i += 1;
                        c = template[i..].chars().next().unwrap_or('\\');
                    }
                    nodes.push(Node::Char(c));
                    i += c.len_utf8();
                }
                continue;
            }
            // Outside quotes a backslash is special only before a double quote.
            let c = if c == '\\' && s.get(i + 1) == Some(&b'"') {
                i += 1;
                '"'
            } else {
                c
            };
            nodes.push(if is_separator_char(c) {
                Node::Separator(c)
            } else if c.is_ascii() && is_space(c as u8) {
                Node::Space(c)
            } else {
                Node::Char(c)
            });
            i += c.len_utf8();
        }
        DateTemplate { nodes }
    }

    /// Whether the template writes the zone abbreviation, with `TZ` or `tz`. The caller needs the
    /// abbreviation of a `timestamptz` only then.
    pub fn names_zone(&self) -> bool {
        self.nodes.iter().any(|node| matches!(node, Node::Action { key, .. } if key.id == Id::Zone))
    }
}

/// `TmToChar`: the fields that `to_char` writes. The hour is 64 bits for an interval.
#[derive(Debug, Default)]
struct ToChar<'a> {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i64,
    min: i32,
    sec: i32,
    yday: i32,
    wday: i32,
    fsec: i32,
    /// Seconds east of UTC.
    gmtoff: i32,
    tzn: Option<&'a str>,
}

/// `INVALID_FOR_INTERVAL`.
fn invalid_for_interval() -> TypeError {
    TypeError {
        hint: Some("Intervals are not tied to specific calendar dates.".to_string()),
        ..TypeError::new(
            SqlState::INVALID_DATETIME_FORMAT,
            "invalid format specification for an interval value".to_string(),
        )
    }
}

/// `sprintf("%0*d")`: the sign counts in the width.
fn number(out: &mut String, value: i64, width: usize) {
    let _ = write!(out, "{value:0width$}");
}

/// `str_numth` on the number that starts at `start`, for the `TH` and `th` suffixes.
fn ordinal(out: &mut String, start: usize, suffix: u8) {
    if suffix & (TH_UPPER | TH_LOWER) == 0 {
        return;
    }
    let digits = &out.as_bytes()[start..];
    let teen = digits.len() > 1 && digits[digits.len() - 2] == b'1';
    let th = match digits.last() {
        Some(b'1') if !teen => "st",
        Some(b'2') if !teen => "nd",
        Some(b'3') if !teen => "rd",
        _ => "th",
    };
    if suffix & TH_UPPER != 0 {
        out.push_str(&th.to_ascii_uppercase());
    } else {
        out.push_str(th);
    }
}

/// `ADJUST_YEAR`: a year BC is written as a positive number, except in an interval.
fn adjust_year(year: i32, interval: bool) -> i32 {
    if interval || year > 0 { year } else { 1i32.wrapping_sub(year) }
}

/// `j2day`: the day of the week of a Julian day, with Sunday 0.
pub(super) fn j2day(date: i32) -> i32 {
    date.wrapping_add(1).rem_euclid(7)
}

/// `isoweek2j`: the Julian day of the Monday of an ISO week.
fn isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4 - 1);
    week.wrapping_sub(1).wrapping_mul(7).wrapping_add(day4 - day0)
}

/// `date2isoweek`.
pub(super) fn date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
    }
    let mut week = (dayn - (day4 - day0)) / 7 + 1;
    if week >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            week = (dayn - (day4 - day0)) / 7 + 1;
        }
    }
    week
}

/// `date2isoyear`.
pub(super) fn date2isoyear(mut year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4 - 1);
    if dayn < day4 - day0 {
        day4 = date2j(year - 1, 1, 4);
        day0 = j2day(day4 - 1);
        year -= 1;
    }
    let week = (dayn - (day4 - day0)) / 7 + 1;
    if week >= 52 {
        day4 = date2j(year + 1, 1, 4);
        day0 = j2day(day4 - 1);
        if dayn >= day4 - day0 {
            year += 1;
        }
    }
    year
}

/// `date2isoyearday`.
fn date2isoyearday(year: i32, mon: i32, mday: i32) -> i32 {
    date2j(year, mon, mday) - isoweek2j(date2isoyear(year, mon, mday), 1) + 1
}

/// `DCH_to_char`.
fn dch_to_char(
    nodes: &[Node],
    interval: bool,
    tm: &ToChar<'_>,
    out: &mut String,
) -> Result<(), TypeError> {
    for node in nodes {
        let (key, suffix, case) = match node {
            Node::Action { key, suffix, case } => (key, *suffix, *case),
            Node::Space(c) | Node::Separator(c) | Node::Char(c) => {
                out.push(*c);
                continue;
            }
        };
        let fm = suffix & FM != 0;
        let fill = |width: usize| if fm { 0 } else { width };
        let signed =
            |value: i64, width: usize| if value >= 0 { fill(width) } else { fill(width + 1) };
        if interval
            && matches!(
                key.id,
                Id::Zone
                    | Id::ZoneHour
                    | Id::ZoneMinute
                    | Id::Offset
                    | Id::Era { .. }
                    | Id::MonthName { .. }
                    | Id::DayName { .. }
                    | Id::DayOfWeek { .. }
            )
        {
            return Err(invalid_for_interval());
        }
        let start = out.len();
        match key.id {
            Id::Meridian { dots } => {
                let pm = tm.hour % 24 >= 12;
                let text = match (dots, pm) {
                    (true, true) => "p.m.",
                    (true, false) => "a.m.",
                    (false, true) => "pm",
                    (false, false) => "am",
                };
                case.apply(text, out);
            }
            Id::Hour12 => {
                let hour = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
                number(out, hour, if tm.hour >= 0 { fill(2) } else { fill(3) });
                ordinal(out, start, suffix);
            }
            Id::Hour24 => {
                number(out, tm.hour, signed(tm.hour, 2));
                ordinal(out, start, suffix);
            }
            Id::Minute => {
                number(out, tm.min.into(), signed(tm.min.into(), 2));
                ordinal(out, start, suffix);
            }
            Id::Second => {
                number(out, tm.sec.into(), signed(tm.sec.into(), 2));
                ordinal(out, start, suffix);
            }
            Id::Fraction(digits) => {
                let digits = usize::from(digits);
                let scale = 10i32.pow(6 - digits as u32);
                number(out, (tm.fsec / scale).into(), digits);
                ordinal(out, start, suffix);
            }
            Id::Millisecond => {
                number(out, (tm.fsec / 1000).into(), 3);
                ordinal(out, start, suffix);
            }
            Id::Microsecond => {
                number(out, tm.fsec.into(), 6);
                ordinal(out, start, suffix);
            }
            Id::SecondOfDay => {
                let seconds = tm
                    .hour
                    .wrapping_mul(3600)
                    .wrapping_add(i64::from(tm.min) * 60)
                    .wrapping_add(tm.sec.into());
                number(out, seconds, 0);
                ordinal(out, start, suffix);
            }
            Id::Zone => {
                if let Some(tzn) = tm.tzn {
                    if tzn.len() > key.name.len() * MAX_ITEM_SIZE {
                        return Err(TypeError::new(
                            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
                            "time zone format value too long".to_string(),
                        ));
                    }
                    // The abbreviations are not localized, so ASCII lower case is enough.
                    if case == Case::Lower {
                        out.extend(tzn.chars().map(|c| c.to_ascii_lowercase()));
                    } else {
                        out.push_str(tzn);
                    }
                }
            }
            Id::ZoneHour => {
                out.push(if tm.gmtoff >= 0 { '+' } else { '-' });
                number(out, (tm.gmtoff.unsigned_abs() / 3600).into(), 2);
            }
            Id::ZoneMinute => {
                number(out, (tm.gmtoff.unsigned_abs() % 3600 / 60).into(), 2);
            }
            Id::Offset => {
                let offset = tm.gmtoff.unsigned_abs();
                out.push(if tm.gmtoff >= 0 { '+' } else { '-' });
                number(out, (offset / 3600).into(), fill(2));
                if !offset.is_multiple_of(3600) {
                    out.push(':');
                    number(out, (offset % 3600 / 60).into(), 2);
                }
            }
            Id::Era { dots } => {
                let text = match (dots, tm.year <= 0) {
                    (true, true) => "b.c.",
                    (true, false) => "a.d.",
                    (false, true) => "bc",
                    (false, false) => "ad",
                };
                case.apply(text, out);
            }
            Id::MonthName { short } => {
                if tm.mon == 0 {
                    continue;
                }
                let index = (tm.mon - 1) as usize;
                if short {
                    case.apply(MONTHS[index], out);
                } else {
                    case.apply(MONTHS_FULL[index], out);
                    pad(out, start, 9, suffix);
                }
            }
            Id::Month => {
                number(out, tm.mon.into(), signed(tm.mon.into(), 2));
                ordinal(out, start, suffix);
            }
            Id::DayName { short } => {
                let index = tm.wday as usize;
                if short {
                    case.apply(DAYS[index], out);
                } else {
                    case.apply(DAYS_FULL[index], out);
                    pad(out, start, 9, suffix);
                }
            }
            Id::DayOfYear { iso } => {
                let day = if iso { date2isoyearday(tm.year, tm.mon, tm.mday) } else { tm.yday };
                number(out, day.into(), fill(3));
                ordinal(out, start, suffix);
            }
            Id::DayOfMonth => {
                number(out, tm.mday.into(), fill(2));
                ordinal(out, start, suffix);
            }
            Id::DayOfWeek { iso } => {
                let day = if iso && tm.wday == 0 { 7 } else { tm.wday + i32::from(!iso) };
                number(out, day.into(), 0);
                ordinal(out, start, suffix);
            }
            Id::Week { iso } => {
                let week = if iso {
                    date2isoweek(tm.year, tm.mon, tm.mday)
                } else {
                    tm.yday.wrapping_sub(1) / 7 + 1
                };
                number(out, week.into(), fill(2));
                ordinal(out, start, suffix);
            }
            Id::Quarter => {
                if tm.mon == 0 {
                    continue;
                }
                number(out, ((tm.mon - 1) / 3 + 1).into(), 0);
                ordinal(out, start, suffix);
            }
            Id::Century => {
                let century = if interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year - 1) / 100 + 1
                } else {
                    tm.year / 100 - 1
                };
                let width =
                    if (-99..=99).contains(&century) { signed(century.into(), 2) } else { 0 };
                number(out, century.into(), width);
                ordinal(out, start, suffix);
            }
            Id::YearComma => {
                let year = adjust_year(tm.year, interval);
                let thousands = year / 1000;
                let _ = write!(out, "{thousands},{:03}", year.wrapping_sub(thousands * 1000));
                ordinal(out, start, suffix);
            }
            Id::Year { width, iso } => {
                let shown = adjust_year(tm.year, interval);
                let year = if iso {
                    adjust_year(date2isoyear(tm.year, tm.mon, tm.mday), interval)
                } else {
                    shown
                };
                let (value, width) = match width {
                    4 => (year, signed(shown.into(), 4)),
                    3 => (year % 1000, signed(shown.into(), 3)),
                    2 => (year % 100, signed(shown.into(), 2)),
                    _ => (year % 10, 1),
                };
                number(out, value.into(), width);
                ordinal(out, start, suffix);
            }
            Id::Roman => {
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                // The array runs from December to January.
                let index = if tm.mon == 0 {
                    if tm.year >= 0 { 0 } else { 11 }
                } else if tm.mon < 0 {
                    -(tm.mon + 1)
                } else {
                    12 - tm.mon
                };
                let months = if case == Case::Lower { &ROMAN_LOWER } else { &ROMAN_UPPER };
                out.push_str(months[index as usize]);
                if !fm {
                    let len = out.len() - start;
                    out.extend(std::iter::repeat_n(' ', 4usize.saturating_sub(len)));
                }
            }
            Id::WeekOfMonth => {
                number(out, (tm.mday.wrapping_sub(1) / 7 + 1).into(), 0);
                ordinal(out, start, suffix);
            }
            Id::Julian => {
                number(out, date2j(tm.year, tm.mon, tm.mday).into(), 0);
                ordinal(out, start, suffix);
            }
            Id::Fx => {}
        }
    }
    Ok(())
}

/// `%-9s` for the full names, unless the `FM` or the `TM` prefix is there.
fn pad(out: &mut String, start: usize, width: usize, suffix: u8) {
    if suffix & (FM | TM) == 0 {
        let len = out.len() - start;
        out.extend(std::iter::repeat_n(' ', width.saturating_sub(len)));
    }
}

/// `timestamp_to_char`, for microseconds since 2000-01-01. `None` for an empty template and for
/// an infinite timestamp.
pub fn timestamp_to_char(ts: i64, template: &DateTemplate) -> Result<Option<String>, TypeError> {
    to_char_at(ts, None, template)
}

/// `timestamptz_to_char`, for microseconds since 2000-01-01, in a zone.
pub fn timestamptz_to_char(
    ts: i64,
    zone: &dyn TimeZone,
    template: &DateTemplate,
) -> Result<Option<String>, TypeError> {
    to_char_at(ts, Some(zone), template)
}

fn to_char_at(
    ts: i64,
    zone: Option<&dyn TimeZone>,
    template: &DateTemplate,
) -> Result<Option<String>, TypeError> {
    if template.nodes.is_empty() || ts == TIMESTAMP_INFINITY || ts == TIMESTAMP_NEGATIVE_INFINITY {
        return Ok(None);
    }
    let range = || out_of_range("timestamp");
    let utc = Fields::of_timestamp(ts).ok_or_else(range)?;
    let mut tm = ToChar { fsec: utc.usec as i32, ..ToChar::default() };
    let (year, month, day, hour, minute, second);
    match zone {
        None => {
            (year, month, day) = (utc.year, utc.month, utc.day);
            (hour, minute, second) = (utc.hour, utc.minute, utc.second);
        }
        Some(zone) => {
            let unix = ts.div_euclid(USECS_PER_SEC) - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC;
            let offset = if template.names_zone() {
                let (offset, name) = zone.at(unix);
                tm.tzn = Some(name);
                offset
            } else {
                zone.offset_at(unix)
            };
            tm.gmtoff = offset;
            let local = unix + i64::from(offset);
            (year, month, day) =
                j2date((local.div_euclid(86400) + i64::from(UNIX_EPOCH_JDATE)) as i32);
            (hour, minute, second, _) = split_time(local.rem_euclid(86400) * USECS_PER_SEC);
        }
    }
    (tm.year, tm.mon, tm.mday) = (year, month as i32, day as i32);
    (tm.hour, tm.min, tm.sec) = (hour.into(), minute as i32, second as i32);
    let thisdate = date2j(tm.year, tm.mon, tm.mday);
    tm.wday = (thisdate + 1) % 7;
    tm.yday = thisdate - date2j(tm.year, 1, 1) + 1;
    let mut out = String::with_capacity(template.nodes.len() * 2);
    dch_to_char(&template.nodes, false, &tm, &mut out)?;
    Ok(Some(out))
}

/// `interval_to_char`. `None` for an empty template and for an infinite interval.
pub fn interval_to_char(
    iv: &Interval,
    template: &DateTemplate,
) -> Result<Option<String>, TypeError> {
    if template.nodes.is_empty() || *iv == Interval::INFINITY || *iv == Interval::NEGATIVE_INFINITY
    {
        return Ok(None);
    }
    // `interval2itm`: each field has the sign of its source, as C division gives it.
    let hour = iv.time / (3600 * USECS_PER_SEC);
    let rest = iv.time - hour * 3600 * USECS_PER_SEC;
    let min = rest / (60 * USECS_PER_SEC);
    let rest = rest - min * 60 * USECS_PER_SEC;
    let sec = rest / USECS_PER_SEC;
    let mut tm = ToChar {
        year: iv.month / 12,
        mon: iv.month % 12,
        mday: iv.day,
        hour,
        min: min as i32,
        sec: sec as i32,
        fsec: (rest - sec * USECS_PER_SEC) as i32,
        ..ToChar::default()
    };
    // The day of the year is about the span in days. The day of the week means nothing.
    tm.yday = tm.year.wrapping_mul(12).wrapping_add(tm.mon).wrapping_mul(30).wrapping_add(tm.mday);
    let mut out = String::with_capacity(template.nodes.len() * 2);
    dch_to_char(&template.nodes, true, &tm, &mut out)?;
    Ok(Some(out))
}

/// `TmFromChar`: the fields that the input gives.
#[derive(Default)]
struct FromChar {
    mode: Mode,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock_12_hour: bool,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
    has_tz: bool,
    /// Seconds east of UTC of a fixed abbreviation.
    gmtoffset: i32,
    /// The zone of an abbreviation whose offset changed over time.
    tzp: Option<ZoneRef>,
    abbrev: String,
}

fn format_error(message: String) -> TypeError {
    TypeError::new(SqlState::INVALID_DATETIME_FORMAT, message)
}

fn not_fixed_width() -> Option<String> {
    Some("If your source string is not fixed-width, try using the \"FM\" modifier.".to_string())
}

/// `from_char_set_int`.
fn set_int(dest: &mut i32, value: i32, key: &Key) -> Result<(), TypeError> {
    if *dest != 0 && *dest != value {
        return Err(TypeError {
            detail: Some(
                "This value contradicts a previous setting for the same field type.".to_string(),
            ),
            ..format_error(format!(
                "conflicting values for \"{}\" field in formatting string",
                key.name
            ))
        });
    }
    *dest = value;
    Ok(())
}

/// The length of the UTF-8 character that starts at a byte, as `pg_mblen` gives it.
fn mblen(b: u8) -> usize {
    match b {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// `adjust_partial_year_to_2020`.
fn adjust_partial_year_to_2020(year: i32) -> i32 {
    match year {
        ..70 => year + 2000,
        70..100 => year + 1900,
        100..520 => year + 2000,
        520..1000 => year + 1000,
        _ => year,
    }
}

/// The reader of `DCH_from_char` over the input.
struct Reader<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn at(&self, i: usize) -> u8 {
        self.s.get(i).copied().unwrap_or(0)
    }

    fn peek(&self) -> u8 {
        self.at(self.pos)
    }

    fn rest(&self) -> String {
        String::from_utf8_lossy(&self.s[self.pos.min(self.s.len())..]).into_owned()
    }

    fn skip_char(&mut self) {
        self.pos = (self.pos + mblen(self.peek())).min(self.s.len());
    }

    /// `SKIP_THth`: the two characters of an ordinal suffix after a number.
    fn skip_th(&mut self, suffix: u8) {
        if suffix & (TH_UPPER | TH_LOWER) != 0 {
            for _ in 0..2 {
                if self.peek() != 0 {
                    self.skip_char();
                }
            }
        }
    }

    /// `from_char_parse_int_len`: a number of at most `len` characters, or as many as there are
    /// in fill mode and before a separator. The value and the characters read, with the spaces.
    fn parse_int_len(
        &mut self,
        len: usize,
        nodes: &[Node],
        at: usize,
    ) -> Result<(i32, usize), TypeError> {
        let Node::Action { key, suffix, .. } = &nodes[at] else { unreachable!() };
        let init = self.pos;
        // Only blanks, as `strspn(s, " ")` takes them.
        while self.peek() == b' ' {
            self.pos += 1;
        }
        let remain = self.s.len() - self.pos;
        let copy = String::from_utf8_lossy(&self.s[self.pos..self.pos + remain.min(len)]);
        let (result, range) = if suffix & FM != 0 || is_next_separator(nodes, at) {
            let (value, end, range) = strtol(self.s, init);
            self.pos = end;
            (value, range)
        } else {
            if remain < len {
                return Err(TypeError {
                    detail: Some(format!(
                        "Field requires {len} characters, but only {remain} remain."
                    )),
                    hint: not_fixed_width(),
                    ..format_error(format!(
                        "source string too short for \"{}\" formatting field",
                        key.name
                    ))
                });
            }
            let (value, end, range) = strtol(copy.as_bytes(), 0);
            if end > 0 && end < len {
                return Err(TypeError {
                    detail: Some(format!(
                        "Field requires {len} characters, but only {end} could be parsed."
                    )),
                    hint: not_fixed_width(),
                    ..format_error(format!("invalid value \"{copy}\" for \"{}\"", key.name))
                });
            }
            self.pos += end;
            (value, range)
        };
        if self.pos == init {
            return Err(TypeError {
                detail: Some("Value must be an integer.".to_string()),
                ..format_error(format!("invalid value \"{copy}\" for \"{}\"", key.name))
            });
        }
        let Ok(result) = i32::try_from(result)
            .map_err(|_| ())
            .and_then(|value| if range { Err(()) } else { Ok(value) })
        else {
            return Err(TypeError {
                detail: Some(format!("Value must be in the range {} to {}.", i32::MIN, i32::MAX)),
                ..TypeError::new(
                    SqlState::DATETIME_VALUE_OUT_OF_RANGE,
                    format!("value for \"{}\" in source string is out of range", key.name),
                )
            });
        };
        Ok((result, self.pos - init))
    }

    /// `from_char_seq_search`: the index of the first name that the input starts with, in any
    /// case.
    fn seq_search(&mut self, names: &[&str], key: &Key) -> Result<usize, TypeError> {
        let rest = &self.s[self.pos..];
        let found = names.iter().position(|name| {
            rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name.as_bytes())
        });
        match found {
            Some(index) => {
                self.pos += names[index].len();
                Ok(index)
            }
            None => {
                // The report stops at the next space, so that it does not show what follows.
                let end = rest
                    .iter()
                    .position(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
                    .unwrap_or(rest.len());
                Err(TypeError {
                    detail: Some(
                        "The given value did not match any of the allowed values for this field."
                            .to_string(),
                    ),
                    ..format_error(format!(
                        "invalid value \"{}\" for \"{}\"",
                        String::from_utf8_lossy(&rest[..end]),
                        key.name
                    ))
                })
            }
        }
    }
}

/// `is_next_separator`: whether the input of a number field ends at a non-digit.
fn is_next_separator(nodes: &[Node], at: usize) -> bool {
    if let Node::Action { suffix, .. } = &nodes[at]
        && suffix & (TH_UPPER | TH_LOWER) != 0
    {
        return true;
    }
    match nodes.get(at + 1) {
        None => true,
        Some(Node::Action { key, .. }) => !key.digit,
        Some(Node::Space(c) | Node::Separator(c) | Node::Char(c)) => !c.is_ascii_digit(),
    }
}

/// `DecodeTimezoneAbbrevPrefix`: the length of the longest zone abbreviation at the start of the
/// input, its offset east of UTC, and its zone when its offset changed over time.
fn zone_prefix(s: &[u8], cx: &DateTimeInput<'_>) -> Option<(usize, i32, Option<ZoneRef>)> {
    let len = s.iter().take(TOKMAXLEN).take_while(|b| b.is_ascii_alphabetic()).count();
    let lower = String::from_utf8_lossy(&s[..len]).to_ascii_lowercase();
    for len in (1..=len).rev() {
        let token = &lower[..len];
        match cx.zone.abbrev_meaning(&token.to_ascii_uppercase()) {
            Some(AbbrevMeaning::Fixed { offset, .. }) => return Some((len, offset, None)),
            Some(AbbrevMeaning::Varies) => return Some((len, 0, Some(ZoneRef::Session))),
            None => {}
        }
        match cx.abbrevs.get(token) {
            Some((_, Abbrev::Fixed { offset, .. })) => return Some((len, *offset, None)),
            Some((_, Abbrev::Zone(name))) => {
                if let Some(zone) = cx.zones.zone(name) {
                    return Some((len, 0, Some(ZoneRef::Other(zone))));
                }
            }
            None => {}
        }
    }
    None
}

/// `DCH_from_char`, outside the standard mode of the SQL/JSON functions.
fn dch_from_char(
    nodes: &[Node],
    input: &[u8],
    cx: &DateTimeInput<'_>,
) -> Result<FromChar, TypeError> {
    let mut out = FromChar::default();
    let mut r = Reader { s: input, pos: 0 };
    let mut fx_mode = false;
    // The characters skipped beyond the ones that the template gives.
    let mut extra_skip = 0i32;
    let mut at = 0;
    while at < nodes.len() && r.peek() != 0 {
        let node = &nodes[at];
        let action = matches!(node, Node::Action { .. });
        let fx = matches!(node, Node::Action { key, .. } if key.id == Id::Fx);
        // Spaces at the start and before a field are ignored, unless in FX mode.
        if !fx_mode && !fx && (action || at == 0) {
            while is_space(r.peek()) {
                r.pos += 1;
                extra_skip += 1;
            }
        }
        let (key, suffix) = match node {
            Node::Space(_) | Node::Separator(_) => {
                if !fx_mode {
                    // One space or separator of the template matches one of the input, or
                    // nothing.
                    extra_skip -= 1;
                    let b = r.peek();
                    if is_space(b) || is_separator_char(char::from(b)) {
                        r.pos += 1;
                        extra_skip += 1;
                    }
                } else {
                    r.skip_char();
                }
                at += 1;
                continue;
            }
            Node::Char(_) => {
                // The input character is consumed and not compared. Out of FX mode an extra
                // skipped character is taken as this one, since it might belong to a field.
                if !fx_mode && extra_skip > 0 {
                    extra_skip -= 1;
                } else {
                    r.skip_char();
                }
                at += 1;
                continue;
            }
            Node::Action { key, suffix, .. } => (*key, *suffix),
        };
        if key.mode != Mode::None {
            if out.mode == Mode::None {
                out.mode = key.mode;
            } else if out.mode != key.mode {
                return Err(TypeError {
                    hint: Some(
                        "Do not mix Gregorian and ISO week date conventions in a formatting template."
                            .to_string(),
                    ),
                    ..format_error("invalid combination of date conventions".to_string())
                });
            }
        }
        let here = at;
        let parse = move |r: &mut Reader<'_>, len: usize| r.parse_int_len(len, nodes, here);
        let keylen = key.name.len();
        match key.id {
            Id::Fx => fx_mode = true,
            Id::Meridian { dots } => {
                let index = r.seq_search(if dots { &AMPM_LONG } else { &AMPM }, key)?;
                set_int(&mut out.pm, (index % 2) as i32, key)?;
                out.clock_12_hour = true;
            }
            Id::Hour12 => {
                let (value, _) = parse(&mut r, 2)?;
                set_int(&mut out.hh, value, key)?;
                out.clock_12_hour = true;
                r.skip_th(suffix);
            }
            Id::Hour24 => {
                let (value, _) = parse(&mut r, 2)?;
                set_int(&mut out.hh, value, key)?;
                r.skip_th(suffix);
            }
            Id::Minute => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.mi, value, key)?;
                r.skip_th(suffix);
            }
            Id::Second => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.ss, value, key)?;
                r.skip_th(suffix);
            }
            Id::Millisecond => {
                let (value, len) = parse(&mut r, 3)?;
                set_int(&mut out.ms, value, key)?;
                // 25 is 0.25 and 250 is 0.25 too, but 025 is 0.025.
                out.ms = out.ms.wrapping_mul(match len {
                    1 => 100,
                    2 => 10,
                    _ => 1,
                });
                r.skip_th(suffix);
            }
            Id::Fraction(_) | Id::Microsecond => {
                if let Id::Fraction(digits) = key.id {
                    out.ff = digits.into();
                }
                let len = if key.id == Id::Microsecond { 6 } else { out.ff as usize };
                let (value, len) = parse(&mut r, len)?;
                set_int(&mut out.us, value, key)?;
                out.us = out.us.wrapping_mul(match len {
                    1 => 100_000,
                    2 => 10_000,
                    3 => 1000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                });
                r.skip_th(suffix);
            }
            Id::SecondOfDay => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.ssss, value, key)?;
                r.skip_th(suffix);
            }
            Id::Zone | Id::Offset => {
                if key.id == Id::Zone {
                    if let Some((len, offset, zone)) = zone_prefix(&r.s[r.pos..], cx) {
                        out.has_tz = true;
                        out.gmtoffset = offset;
                        // Only an abbreviation with a zone needs its text.
                        if zone.is_some() {
                            out.abbrev = String::from_utf8_lossy(&r.s[r.pos..r.pos + len]).into();
                        }
                        out.tzp = zone;
                        // An abbreviation drops any earlier TZH and TZM.
                        out.tzsign = 0;
                        r.pos += len;
                        at += 1;
                        skip_after_field(&mut r, fx_mode, &mut extra_skip);
                        continue;
                    }
                    if r.peek().is_ascii_alphabetic() {
                        return Err(TypeError {
                            detail: Some("Time zone abbreviation is not recognized.".to_string()),
                            ..format_error(format!(
                                "invalid value \"{}\" for \"{}\"",
                                r.rest(),
                                key.name
                            ))
                        });
                    }
                }
                // OF is TZH, or TZH:TZM.
                out.tzsign = zone_sign(&mut r, extra_skip);
                let (value, _) = parse(&mut r, 2)?;
                set_int(&mut out.tzh, value, key)?;
                if r.peek() == b':' {
                    r.pos += 1;
                    let (value, _) = parse(&mut r, 2)?;
                    set_int(&mut out.tzm, value, key)?;
                }
            }
            Id::ZoneHour => {
                // A negative hour might have had its minus sign skipped as a separator, so a
                // skipped minus counts when more characters were skipped than the template has.
                out.tzsign = zone_sign(&mut r, extra_skip);
                let (value, _) = parse(&mut r, 2)?;
                set_int(&mut out.tzh, value, key)?;
            }
            Id::ZoneMinute => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                let (value, _) = parse(&mut r, 2)?;
                set_int(&mut out.tzm, value, key)?;
            }
            Id::Era { dots } => {
                let index = r.seq_search(if dots { &ADBC_LONG } else { &ADBC }, key)?;
                set_int(&mut out.bc, (index % 2) as i32, key)?;
            }
            Id::MonthName { short } => {
                let names: &[&str] = if short { &MONTHS } else { &MONTHS_FULL };
                let index = r.seq_search(names, key)?;
                set_int(&mut out.mm, index as i32 + 1, key)?;
            }
            Id::Month => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.mm, value, key)?;
                r.skip_th(suffix);
            }
            Id::DayName { short } => {
                let names: &[&str] = if short { &DAYS } else { &DAYS_FULL };
                let index = r.seq_search(names, key)?;
                set_int(&mut out.d, index as i32, key)?;
                out.d += 1;
            }
            Id::DayOfYear { iso } => {
                let (value, _) = parse(&mut r, if iso { 3 } else { keylen })?;
                set_int(&mut out.ddd, value, key)?;
                r.skip_th(suffix);
            }
            Id::DayOfMonth => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.dd, value, key)?;
                r.skip_th(suffix);
            }
            Id::DayOfWeek { iso: false } => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.d, value, key)?;
                r.skip_th(suffix);
            }
            Id::DayOfWeek { iso: true } => {
                let (value, _) = parse(&mut r, 1)?;
                set_int(&mut out.d, value, key)?;
                // Sunday is 1, as in the Gregorian numbering.
                out.d = out.d.wrapping_add(1);
                if out.d > 7 {
                    out.d = 1;
                }
                r.skip_th(suffix);
            }
            Id::Week { .. } => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.ww, value, key)?;
                r.skip_th(suffix);
            }
            Id::Quarter => {
                // The quarter is read and not used, since it does not say which date in the
                // quarter to take.
                parse(&mut r, keylen)?;
                r.skip_th(suffix);
            }
            Id::Century => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.cc, value, key)?;
                r.skip_th(suffix);
            }
            Id::YearComma => {
                let Some((millennia, years, nch)) = scan_year_comma(&r.s[r.pos..]) else {
                    return Err(format_error("invalid input string for \"Y,YYY\"".to_string()));
                };
                let Some(years) = millennia.checked_mul(1000).and_then(|m| years.checked_add(m))
                else {
                    return Err(TypeError::new(
                        SqlState::DATETIME_FIELD_OVERFLOW,
                        "value for \"Y,YYY\" in source string is out of range".to_string(),
                    ));
                };
                set_int(&mut out.year, years, key)?;
                out.yysz = 4;
                r.pos += nch;
                r.skip_th(suffix);
            }
            Id::Year { width, .. } => {
                let (value, len) = parse(&mut r, keylen)?;
                set_int(&mut out.year, value, key)?;
                if width < 4 && len < 4 {
                    out.year = adjust_partial_year_to_2020(out.year);
                }
                out.yysz = width.into();
                r.skip_th(suffix);
            }
            Id::Roman => {
                let index = r.seq_search(&ROMAN_LOWER, key)?;
                set_int(&mut out.mm, 12 - index as i32, key)?;
            }
            Id::WeekOfMonth => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.w, value, key)?;
                r.skip_th(suffix);
            }
            Id::Julian => {
                let (value, _) = parse(&mut r, keylen)?;
                set_int(&mut out.j, value, key)?;
                r.skip_th(suffix);
            }
        }
        at += 1;
        skip_after_field(&mut r, fx_mode, &mut extra_skip);
    }
    Ok(out)
}

/// The spaces after a field are ignored, unless in FX mode.
fn skip_after_field(r: &mut Reader<'_>, fx_mode: bool, extra_skip: &mut i32) {
    if !fx_mode {
        *extra_skip = 0;
        while is_space(r.peek()) {
            r.pos += 1;
            *extra_skip += 1;
        }
    }
}

/// The sign of `TZH` and `OF`: a `+`, a `-` or a space, or else a minus that was skipped as an
/// extra separator.
fn zone_sign(r: &mut Reader<'_>, extra_skip: i32) -> i32 {
    match r.peek() {
        b'+' | b' ' => {
            r.pos += 1;
            1
        }
        b'-' => {
            r.pos += 1;
            -1
        }
        _ if extra_skip > 0 && r.pos > 0 && r.at(r.pos - 1) == b'-' => -1,
        _ => 1,
    }
}

/// `sscanf("%d,%03d%n")`: the thousands, the rest of the year and the characters read.
fn scan_year_comma(s: &[u8]) -> Option<(i32, i32, usize)> {
    // `%d` skips spaces, takes a sign and needs a digit.
    let (millennia, end, _) = strtol(s, 0);
    if end == 0 {
        return None;
    }
    if s.get(end) != Some(&b',') {
        return None;
    }
    // `%03d` skips spaces too, and then reads at most three characters with the sign.
    let mut i = end + 1;
    while s.get(i).is_some_and(|&b| is_space(b)) {
        i += 1;
    }
    let field = &s[i..s.len().min(i + 3)];
    let (years, used, _) = strtol(field, 0);
    if used == 0 {
        return None;
    }
    Some((millennia as i32, years as i32, i + used))
}

/// The result of `do_to_timestamp`: the fields, the microseconds, the zone in seconds west of UTC
/// when the input gave one, and the precision that `FF1` to `FF6` gave.
struct ToTimestamp {
    tm: Tm,
    fsec: i32,
    tz: Option<i32>,
    fprec: i32,
}

/// `do_to_timestamp`.
fn do_to_timestamp(
    input: &str,
    template: &DateTemplate,
    cx: &DateTimeInput<'_>,
) -> Result<ToTimestamp, TypeError> {
    let bytes = input.as_bytes();
    let bytes = &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())];
    let mut tmfc = dch_from_char(&template.nodes, bytes, cx)?;
    let overflow = || DtErr::FieldOverflow.into_error(input, "timestamp");
    let mut tm = Tm { mon: 1, mday: 1, ..Tm::default() };
    let mut fsec = 0i32;
    let mut fmask = 0u32;
    if tmfc.ssss != 0 {
        let x = tmfc.ssss;
        (tm.hour, tm.min, tm.sec) = (x / 3600, x % 3600 / 60, x % 60);
    }
    if tmfc.ss != 0 {
        tm.sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm.min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm.hour = tmfc.hh;
    }
    if tmfc.clock_12_hour {
        if !(1..=12).contains(&tm.hour) {
            return Err(TypeError {
                hint: Some("Use the 24-hour clock, or give an hour between 1 and 12.".to_string()),
                ..format_error(format!("hour \"{}\" is invalid for the 12-hour clock", tm.hour))
            });
        }
        if tmfc.pm != 0 && tm.hour < 12 {
            tm.hour += 12;
        } else if tmfc.pm == 0 && tm.hour == 12 {
            tm.hour = 0;
        }
    }
    if tmfc.year != 0 {
        // A year of two digits or less with a century is a year in that century.
        if tmfc.cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                tmfc.cc = -tmfc.cc;
            }
            tm.year = tmfc.year % 100;
            if tm.year != 0 {
                tm.year = if tmfc.cc >= 0 {
                    (tmfc.cc - 1).checked_mul(100).and_then(|c| tm.year.checked_add(c))
                } else {
                    (tmfc.cc + 1)
                        .checked_mul(100)
                        .and_then(|c| c.checked_sub(tm.year))
                        .and_then(|c| c.checked_add(1))
                }
                .ok_or_else(overflow)?;
            } else {
                tm.year = tmfc.cc.wrapping_mul(100).wrapping_add(i32::from(tmfc.cc < 0));
            }
        } else {
            tm.year = tmfc.year;
            if tmfc.bc != 0 {
                tm.year = tm.year.wrapping_neg();
            }
            if tm.year < 0 {
                tm.year += 1;
            }
        }
        fmask |= m(YEAR);
    } else if tmfc.cc != 0 {
        if tmfc.bc != 0 {
            tmfc.cc = -tmfc.cc;
        }
        tm.year = if tmfc.cc >= 0 {
            (tmfc.cc - 1).checked_mul(100).and_then(|y| y.checked_add(1))
        } else {
            tmfc.cc.checked_mul(100).and_then(|y| y.checked_add(1))
        }
        .ok_or_else(overflow)?;
        fmask |= m(YEAR);
    }
    if tmfc.j != 0 {
        set_date(&mut tm, tmfc.j);
        fmask |= DATE_M;
    }
    if tmfc.ww != 0 {
        if tmfc.mode == Mode::IsoWeek {
            let mut julian = isoweek2j(tm.year, tmfc.ww);
            if tmfc.d != 0 {
                julian = julian.wrapping_add(if tmfc.d > 1 { tmfc.d - 2 } else { 6 });
            }
            set_date(&mut tm, julian);
            fmask |= DATE_M;
        } else {
            tmfc.ddd =
                (tmfc.ww - 1).checked_mul(7).and_then(|d| d.checked_add(1)).ok_or_else(overflow)?;
        }
    }
    if tmfc.w != 0 {
        tmfc.dd =
            (tmfc.w - 1).checked_mul(7).and_then(|d| d.checked_add(1)).ok_or_else(overflow)?;
    }
    if tmfc.dd != 0 {
        tm.mday = tmfc.dd;
        fmask |= m(DAY);
    }
    if tmfc.mm != 0 {
        tm.mon = tmfc.mm;
        fmask |= m(MONTH);
    }
    if tmfc.ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
        if tm.year == 0 && tmfc.bc == 0 {
            return Err(format_error(
                "cannot calculate day of year without year information".to_string(),
            ));
        }
        if tmfc.mode == Mode::IsoWeek {
            let j0 = isoweek2j(tm.year, 1) - 1;
            set_date(&mut tm, j0.wrapping_add(tmfc.ddd));
            fmask |= DATE_M;
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[usize::from(is_leap(tm.year))];
            let i = (1..=12).find(|&i| tmfc.ddd <= y[i]).unwrap_or(13);
            if tm.mon <= 1 {
                tm.mon = i as i32;
            }
            if tm.mday <= 1 {
                tm.mday = tmfc.ddd - y[i - 1];
            }
            fmask |= m(MONTH) | m(DAY);
        }
    }
    if tmfc.ms != 0 {
        fsec =
            tmfc.ms.checked_mul(1000).and_then(|ms| fsec.checked_add(ms)).ok_or_else(overflow)?;
    }
    if tmfc.us != 0 {
        fsec = fsec.wrapping_add(tmfc.us);
    }
    if fmask != 0 && validate_date(fmask, true, false, false, &mut tm).is_err() {
        return Err(overflow());
    }
    if !(0..24).contains(&tm.hour)
        || !(0..60).contains(&tm.min)
        || !(0..60).contains(&tm.sec)
        || !(0..1_000_000).contains(&fsec)
    {
        return Err(overflow());
    }
    let tz = if tmfc.tzsign != 0 {
        if !(0..=MAX_TZDISP_HOUR).contains(&tmfc.tzh) || !(0..60).contains(&tmfc.tzm) {
            return Err(DtErr::TzDispOverflow.into_error(input, "timestamp"));
        }
        let offset = (tmfc.tzh * 60 + tmfc.tzm) * 60;
        Some(if tmfc.tzsign > 0 { -offset } else { offset })
    } else if tmfc.has_tz {
        Some(match &tmfc.tzp {
            None => -tmfc.gmtoffset,
            Some(zone) => determine_abbrev_offset(&tm, tmfc.abbrev.as_bytes(), zone.get(cx.zone)),
        })
    } else {
        None
    };
    Ok(ToTimestamp { tm, fsec, tz, fprec: tmfc.ff })
}

/// `to_timestamp(text, text)`: microseconds since 2000-01-01. A time with no zone in the input is
/// in the session zone.
pub fn to_timestamp(
    input: &str,
    template: &DateTemplate,
    cx: &DateTimeInput<'_>,
) -> Result<i64, TypeError> {
    let parsed = do_to_timestamp(input, template, cx)?;
    let tz = match parsed.tz {
        Some(tz) => tz,
        None => determine_offset(&parsed.tm, cx.zone).0,
    };
    let ts =
        tm2timestamp(&parsed.tm, parsed.fsec, Some(tz)).ok_or_else(|| out_of_range("timestamp"))?;
    if parsed.fprec != 0 { adjust_timestamp(ts, parsed.fprec) } else { Ok(ts) }
}

/// `to_date(text, text)`: days since 2000-01-01.
pub fn to_date(
    input: &str,
    template: &DateTemplate,
    cx: &DateTimeInput<'_>,
) -> Result<i32, TypeError> {
    let tm = do_to_timestamp(input, template, cx)?.tm;
    let range = || {
        TypeError::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            format!("date out of range: \"{input}\""),
        )
    };
    if !is_valid_julian(tm.year, tm.mon) {
        return Err(range());
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !(-POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE).contains(&date) {
        return Err(range());
    }
    Ok(date)
}

#[cfg(test)]
mod tests {
    use rudb_common::SessionTimeZone;

    use super::*;
    use crate::datetime::decode::{NoZones, ZoneAbbrevs, timestamp_in};
    use crate::datetime::{DateFormat, DateOrder, FixedZone, timestamp_out};

    fn input(zone: &dyn TimeZone) -> DateTimeInput<'_> {
        DateTimeInput {
            order: DateOrder::Mdy,
            zone,
            zones: &NoZones,
            abbrevs: ZoneAbbrevs::postgres_default(),
            now: 0,
        }
    }

    fn timestamp(text: &str) -> i64 {
        timestamp_in(text, -1, &input(&FixedZone::utc())).unwrap()
    }

    fn shown(ts: i64) -> String {
        let mut out = Vec::new();
        timestamp_out(ts, DateFormat::ISO_MDY, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn error(result: Result<impl std::fmt::Debug, TypeError>) -> (SqlState, String) {
        let error = result.unwrap_err();
        (error.sqlstate, error.message)
    }

    // The answers below are the answers of the PostgreSQL 19 server.
    #[test]
    fn to_char_writes_a_timestamp_as_postgresql_writes_it() {
        for (value, template, written) in [
            (
                "2024-03-10 12:34:56.789012",
                "YYYY-MM-DD HH24:MI:SS.US",
                "2024-03-10 12:34:56.789012",
            ),
            (
                "2024-03-10 13:04:06",
                "HH HH12 HH24 AM am A.M. p.m. PM",
                "01 01 13 PM pm P.M. p.m. PM",
            ),
            (
                "2024-03-10 12:34:56.789012",
                "FF1 FF2 FF3 FF4 FF5 FF6 MS US SSSS SSSSS",
                "7 78 789 7890 78901 789012 789 789012 45296 45296",
            ),
            (
                "2024-03-10",
                "MONTH Month month MON Mon mon|",
                "MARCH     March     march     MAR Mar mar|",
            ),
            (
                "2024-03-10",
                "DAY Day day DY Dy dy|FMDay|",
                "SUNDAY    Sunday    sunday    SUN Sun sun|Sunday|",
            ),
            (
                "2024-03-10",
                "DDD DD D ID IDDD IW WW W Q J CC",
                "070 10 1 7 070 10 10 2 1 2460380 21",
            ),
            (
                "2024-03-10",
                "RM rm FMRM| BC AD bc ad B.C. a.d.",
                "III  iii  III| AD AD ad ad A.D. a.d.",
            ),
            (
                "2024-03-01 12:34:56",
                "DDth DDTH FMDDth Dth MMth YYYYth HH24th",
                "01st 01ST 1st 6th 03rd 2024th 12th",
            ),
            ("0044-03-15 BC", "YYYY BC CC Y,YYY YY", "0044 BC -01 0,044 44"),
            (
                "2024-03-10",
                r#""Year" YYYY "quoted \" text" \"Q\" Q"#,
                r#"Year 2024 quoted " text "1" 1"#,
            ),
            ("2024-03-10", "Hello World! abcdefg", "Hello 2orl1! aad1efg"),
            ("2024-03-10", "é Ünïcode ü", "é Ünïco1e ü"),
            ("12345-03-10", "YYYY YYY Y,YYY CC", "12345 345 12,345 124"),
            ("2021-01-01", "IYYY IW ID IDDD YYYY WW DDD", "2020 53 5 369 2021 01 001"),
            (
                "2024-03-10",
                "FMHH FMHH24 FMMI FMSS FMDD FMMM FMYYYY FMCC FMIW FMDDD",
                "12 0 0 0 10 3 2024 21 10 70",
            ),
            ("2024-03-10", "TZ tz OF TZH TZM", "  +00 +00 00"),
        ] {
            let template = DateTemplate::parse(template);
            let answer = timestamp_to_char(timestamp(value), &template).unwrap();
            assert_eq!(answer.as_deref(), Some(written), "{value} {template:?}");
        }
        let empty = DateTemplate::parse("");
        assert_eq!(timestamp_to_char(timestamp("2024-03-10"), &empty), Ok(None));
        let year = DateTemplate::parse("YYYY");
        assert_eq!(timestamp_to_char(TIMESTAMP_INFINITY, &year), Ok(None));
    }

    #[test]
    fn to_char_writes_a_timestamptz_in_the_session_zone() {
        let instant = timestamp("2024-07-10 12:34:56");
        let template = DateTemplate::parse("YYYY-MM-DD HH24:MI TZ tz OF FMOF TZH:TZM");
        for (zone, written) in [
            ("Europe/Paris", "2024-07-10 14:34 CEST cest +02 +2 +02:00"),
            ("Asia/Kolkata", "2024-07-10 18:04 IST ist +05:30 +5:30 +05:30"),
            ("America/Sao_Paulo", "2024-07-10 09:34 -03 -03 -03 -3 -03:00"),
            ("XYZ+3", "2024-07-10 09:34 XYZ xyz -03 -3 -03:00"),
        ] {
            let zone = SessionTimeZone::of_postgres(zone).unwrap();
            let answer = timestamptz_to_char(instant, &zone, &template).unwrap();
            assert_eq!(answer.as_deref(), Some(written));
        }
    }

    #[test]
    fn to_char_writes_an_interval_with_the_fields_that_an_interval_has() {
        let template = DateTemplate::parse("YYYY MM DD HH24 MI SS MS DDD Y,YYY CC Q RM W J");
        let interval = Interval { month: 14, day: 3, time: 14_706_789_000 };
        assert_eq!(
            interval_to_char(&interval, &template).unwrap().as_deref(),
            Some("0001 02 03 04 05 06 789 423 0,001 00 1 II   1 1721459")
        );
        let template = DateTemplate::parse("YYYY MM DD HH24 HH12 MI SS RM");
        let interval = Interval { month: -14, day: -3, time: -14_706_000_000 };
        assert_eq!(
            interval_to_char(&interval, &template).unwrap().as_deref(),
            Some("-0001 -02 -3 -04 -04 -05 -06 XI  ")
        );
        let hours = Interval { month: 0, day: 0, time: 360_000_000_000 };
        let template = DateTemplate::parse("HH24 HH12 HH SSSS AM");
        assert_eq!(
            interval_to_char(&hours, &template).unwrap().as_deref(),
            Some("100 04 04 360000 AM")
        );
        for template in ["Day", "TZ"] {
            let (state, message) = error(interval_to_char(&hours, &DateTemplate::parse(template)));
            assert_eq!(state, SqlState::INVALID_DATETIME_FORMAT);
            assert_eq!(message, "invalid format specification for an interval value");
        }
    }

    #[test]
    fn to_timestamp_reads_a_string_with_its_template() {
        let utc = FixedZone::utc();
        for (text, template, value) in [
            (
                "2024-03-10 12:34:56.789012",
                "YYYY-MM-DD HH24:MI:SS.US",
                "2024-03-10 12:34:56.789012",
            ),
            ("10 Mar 2024 01:02 PM", "DD Mon YYYY HH:MI AM", "2024-03-10 13:02:00"),
            ("12 a.m.", "HH a.m.", "0001-01-01 00:00:00 BC"),
            ("2024 070", "YYYY DDD", "2024-03-10 00:00:00"),
            ("2024 10 1", "IYYY IW ID", "2024-03-04 00:00:00"),
            ("2460380", "J", "2024-03-10 00:00:00"),
            ("21 24", "CC YY", "2024-01-01 00:00:00"),
            ("21", "CC", "2001-01-01 00:00:00"),
            ("44 BC", "YYYY BC", "0044-01-01 00:00:00 BC"),
            ("1,234", "Y,YYY", "1234-01-01 00:00:00"),
            ("24-3-5", "YY-MM-DD", "2024-03-05 00:00:00"),
            ("2024-03-10 12:34:56 +05:30", "YYYY-MM-DD HH24:MI:SS TZH:TZM", "2024-03-10 07:04:56"),
            ("2024-03-10 12:34:56 -05", "YYYY-MM-DD HH24:MI:SS TZH", "2024-03-10 17:34:56"),
        ] {
            let answer = to_timestamp(text, &DateTemplate::parse(template), &input(&utc));
            assert_eq!(answer.map(shown), Ok(value.to_owned()), "{text} {template}");
        }
        // A string with no zone in it is read in the zone of the session.
        let new_york = SessionTimeZone::of_postgres("America/New_York").unwrap();
        let template = DateTemplate::parse("YYYY-MM-DD HH24:MI");
        let answer = to_timestamp("2024-07-01 12:00", &template, &input(&new_york));
        assert_eq!(answer, Ok(timestamp("2024-07-01 16:00")));
        let template = DateTemplate::parse("YYYY-MM-DD HH24MI");
        let (state, message) = error(to_timestamp("2024-07-01 1", &template, &input(&utc)));
        assert_eq!(state, SqlState::INVALID_DATETIME_FORMAT);
        assert_eq!(message, "source string too short for \"HH24\" formatting field");
    }

    #[test]
    fn to_date_reads_the_date_of_a_string() {
        let utc = FixedZone::utc();
        let template = DateTemplate::parse("YYYYMMDD");
        assert_eq!(to_date("20000102", &template, &input(&utc)), Ok(1));
        let (state, message) =
            error(to_date("5874898-01-01", &DateTemplate::parse("YYYY-MM-DD"), &input(&utc)));
        assert_eq!(state, SqlState::DATETIME_VALUE_OUT_OF_RANGE);
        assert_eq!(message, "date out of range: \"5874898-01-01\"");
    }
}
