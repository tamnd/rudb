//! What type a column of text is.
//!
//! A CSV file has no types in it, so a reader that wants to hand a `BIGINT` to an aggregate has to
//! decide, and a wrong decision is a wrong answer rather than a slow query. The rule is DuckDB's
//! and it is a ladder of types, tried in turn, with `VARCHAR` at the bottom because everything fits
//! it. A column of nothing but nulls is `VARCHAR` too, which is the bottom of the same ladder.
//!
//! The ladder is walked a value at a time, which is the pin's `CSVSniffer::SniffTypes`. A column
//! goes onto the first rung its first value fits, and each value after that moves it down to the
//! first rung below that it fits, except that a column on `BOOLEAN`, `TIME`, `DATE` or
//! `TIMESTAMP WITH TIME ZONE` that meets a value which does not fit falls straight to `VARCHAR`,
//! and so does one on `TIMESTAMP` unless the rung under it is `TIMESTAMP WITH TIME ZONE`. So
//! `2020-01-02` over `2020-01-03 03:04:05` is a `VARCHAR` column, the other way round is a
//! `TIMESTAMP` one, and `2020-01-03 03:04:05` over `2020-01-03 03:04:05+02` is a `TIMESTAMP WITH
//! TIME ZONE` one, all measured on `v2.0.0-dev84237`.
//!
//! The order is the pin's `auto_type_candidates` read from the back, `BOOLEAN`, `TIME`, `DATE`,
//! `TIMESTAMP`, `TIMESTAMP WITH TIME ZONE`, `BIGINT`, `HUGEINT`, `BIGNUM` and `DOUBLE`, and a call
//! that sets its own candidates gets them in the same order, which [`ladder`] puts them in.
//! `BOOLEAN` before `BIGINT` matters, because a column of `true` and `false` is a boolean and not a
//! pair of words. `BIGINT` before `DOUBLE` matters, because a
//! column of whole numbers should be whole. `DATE` before `TIMESTAMP` is the order the binary uses,
//! and those two do overlap because a cast to `DATE` reads a time on the end and throws it away, so
//! `timed` below keeps a timestamp off the `DATE` rung.
//!
//! A date written with its day or month first is not one the cast reads, so the two rungs that
//! read days try formats as well, which is `Sniffer::detect`. A value that starts like a date,
//! three numbers with the same separator between them, is tried against the pin's list of formats
//! for that separator, last first, and the first one that reads it is the format the rest of the
//! column, and every later column, is held to. `05/06/2019` is the fifth of June that way, and
//! `13/01/2020` under it agrees, where `01/13/2020` under it does not and the column is text.
//!
//! Six of the rules are the sniffer's rather than the cast's, and all six were measured. `007`
//! is a `VARCHAR` here although `CAST('007' AS BIGINT)` is 7, because a column of zero padded
//! numbers is a column of codes and adding them up is not what anybody meant. `+1` is a `VARCHAR`
//! for the same sort of reason. A day with a time on it is a `TIMESTAMP` and not a `DATE`, and a
//! clock with anything else around it is a `VARCHAR` and not a `TIME`, although the casts to `DATE`
//! and to `TIME` take both. A point or an exponent keeps a value off the `BIGINT` rung, so a column
//! of `1.5` is a `DOUBLE` column although the cast reads that as two, and a separator keeps it off
//! both number rungs, so a column of `1_000` is a `VARCHAR` column although the cast reads that as a
//! thousand. Everything else defers to the cast, which is the point: a string the sniffer calls a
//! `BIGINT` is a string the reader then casts to `BIGINT`, so a test that disagreed with the cast
//! would produce a column whose declared type its own values do not fit.
//!
//! There is one column where upstream does disagree with itself and this does not follow it. A
//! column holding both `0x10` and `1.5` sniffs as `DOUBLE` there, because its `DOUBLE` rung takes a
//! radix that its cast to `DOUBLE` then refuses, so reading the file raises. Here the rung defers to
//! the cast, the column comes out `VARCHAR`, and the file reads.

use rudb_common::{LogicalType, Value};
use rudb_kernels::strftime::Format;
use rudb_kernels::strptime::{fits_strictly, try_date, try_timestamp};
use rudb_kernels::{cast_value, plain_timestamp};

/// The types tried, in the order they are tried. `VARCHAR` is the bottom and is not in here because
/// it never fails.
pub const LADDER: [LogicalType; 9] = [
    LogicalType::Boolean,
    LogicalType::Time,
    LogicalType::Date,
    LogicalType::Timestamp,
    LogicalType::TimestampTz,
    LogicalType::BigInt,
    LogicalType::HugeInt,
    LogicalType::BigNum,
    LogicalType::Double,
];

/// Where a type goes among the sniffer's candidates, the pin's `auto_type_candidates_specificity`,
/// or `None` for a type that cannot be one. The higher it is the earlier the type is tried.
#[must_use]
pub fn specificity(ty: &LogicalType) -> Option<u8> {
    Some(match ty {
        LogicalType::Varchar => 0,
        LogicalType::Double => 1,
        LogicalType::Float => 2,
        LogicalType::Decimal { .. } => 3,
        LogicalType::BigNum => 4,
        LogicalType::HugeInt => 5,
        LogicalType::BigInt => 6,
        LogicalType::Integer => 7,
        LogicalType::SmallInt => 8,
        LogicalType::TinyInt => 9,
        LogicalType::TimestampTz => 10,
        LogicalType::Timestamp => 11,
        LogicalType::Date => 12,
        LogicalType::Time => 13,
        LogicalType::Boolean => 14,
        LogicalType::Null => 15,
        _ => return None,
    })
}

/// The ladder a call's `auto_type_candidates` make, the most specific first.
///
/// Each place holds one type, so of two decimals the later one stays, and `VARCHAR` and `NULL`
/// are left out, because every column ends on the first if nothing above it fits and no value
/// fits the second. A type [`specificity`] has no place for is left out too, and the binder has
/// refused it before this is asked.
#[must_use]
pub fn ladder(candidates: &[LogicalType]) -> Vec<LogicalType> {
    let mut placed = std::collections::BTreeMap::new();
    for ty in candidates {
        if let Some(place) = specificity(ty) {
            placed.insert(place, ty.clone());
        }
    }
    placed
        .into_iter()
        .rev()
        .filter(|(place, _)| !matches!(place, 0 | 15))
        .map(|(_, ty)| ty)
        .collect()
}

/// How many rows the sniffer looks at.
///
/// DuckDB's `sample_size` default, which it prints in the block under a conversion error. A value
/// past this that does not fit the type the sample chose is an error at read time rather than a
/// wider type, because widening would mean going back and rewriting the chunks already handed out.
pub const SAMPLE: usize = 20480;

/// The formats a date and a timestamp in the file are read with, which is the pin's
/// `date_format` and `timestamp_format`. `None` is the cast, which reads `2019-06-05`.
#[derive(Clone, Debug, Default)]
pub struct Clocks {
    pub date: Option<Written>,
    pub timestamp: Option<Written>,
}

/// A date or timestamp format, as it was written and taken apart.
#[derive(Clone, Debug)]
pub struct Written {
    /// The format as it was written, which the block under an error shows.
    pub text: String,
    format: Format,
}

impl Written {
    /// Takes a format apart.
    ///
    /// # Errors
    ///
    /// The reason it does not parse, in the words the pin puts after `Could not parse
    /// DATEFORMAT: `.
    pub fn parse(text: &str) -> Result<Self, String> {
        Ok(Self { text: text.to_string(), format: Format::parsed(text)? })
    }

    /// The day `text` is in this format, or `None` when it is not one.
    #[must_use]
    pub fn date(&self, text: &str) -> Option<i32> {
        try_date(&self.format, text)
    }

    /// The timestamp `text` is in this format, in microseconds, or `None` when it is not one.
    #[must_use]
    pub fn timestamp(&self, text: &str) -> Option<i64> {
        try_timestamp(&self.format, text)
    }
}

/// The type of a column, given every value the sample had for it, in the order the file has them.
///
/// A `None` is a null and is skipped, because a null fits every type and a column that is all nulls
/// has nothing to go on.
#[must_use]
pub fn column(values: &[Option<&str>]) -> LogicalType {
    Sniffer::new(&Clocks::default(), true).column(values, &LADDER)
}

/// The type of each of `columns`, each given the way [`column`] takes one, and the formats the
/// dates and timestamps among them are read with.
///
/// `given` is the formats the call set, which the sniffer holds the columns to rather than trying
/// its own, and with `detect` off it does not look for any either. A format the sniffer found is
/// only kept when a column came out of that type, which is the pin's `SetResultOptions`, so a file
/// with no dates in it has no date format either.
///
/// `set` is the types the call set for the columns, which come back as they are. Their values are
/// still looked at for a format, which is what the pin does: `05/06/2019` over `13/01/2020` loaded
/// into a `DATE` column is the fifth of June and the thirteenth of January. The other columns walk
/// down `ladder`, which is [`LADDER`] unless the call set its own.
#[must_use]
pub fn sniff(
    columns: &[Vec<Option<&str>>],
    set: Option<&[LogicalType]>,
    given: &Clocks,
    detect: bool,
    ladder: &[LogicalType],
) -> (Vec<LogicalType>, Clocks) {
    let mut sniffer = Sniffer::new(given, detect);
    let types: Vec<LogicalType> = columns
        .iter()
        .enumerate()
        .map(|(at, values)| match set.and_then(|set| set.get(at)) {
            Some(LogicalType::Varchar) => LogicalType::Varchar,
            Some(ty) => {
                sniffer.column(values, std::slice::from_ref(ty));
                ty.clone()
            }
            None => sniffer.column(values, ladder),
        })
        .collect();
    let kept = |kind: Kind| {
        given.of(kind).or_else(|| sniffer.current.of(kind).filter(|_| types.contains(&kind.ty())))
    };
    let clocks =
        Clocks { date: kept(Kind::Date).cloned(), timestamp: kept(Kind::Timestamp).cloned() };
    (types, clocks)
}

/// Whether one value would read as `candidate`, with the dates and timestamps in `clocks`.
#[must_use]
pub fn fits_in(text: &str, candidate: &LogicalType, clocks: &Clocks) -> bool {
    match (candidate, clocks) {
        (LogicalType::Date, Clocks { date: Some(format), .. }) => format.date(text).is_some(),
        (LogicalType::Timestamp, Clocks { timestamp: Some(format), .. }) => {
            format.timestamp(text).is_some()
        }
        _ => fits(text, candidate),
    }
}

/// Whether one value would read as `candidate`.
///
/// A `TIMESTAMP` takes no offset and no zone but `UTC` here, although the cast takes both and
/// throws them away, which is the pin's strict parse and leaves those values for the `TIMESTAMP
/// WITH TIME ZONE` under it. A decimal goes to the cast with none of the rules the other numbers
/// have, which was measured: `007`, `+1` and `1e3` are all decimals to the pin.
#[must_use]
pub fn fits(text: &str, candidate: &LogicalType) -> bool {
    use LogicalType::{
        BigInt, BigNum, Boolean, Date, Double, Float, HugeInt, Integer, SmallInt, Time, Timestamp,
        TinyInt,
    };
    match candidate {
        Boolean => is_boolean(text),
        TinyInt | SmallInt | Integer | BigInt | HugeInt if !numeric(text) || !whole(text) => false,
        BigNum => bignum(text),
        Float | Double if !numeric(text) => false,
        Date if timed(text) => false,
        Time if !clock(text) => false,
        Timestamp if !plain_timestamp(text) => false,
        _ => {
            let value = Value::Varchar(text.to_string());
            matches!(cast_value(&value, candidate, true), Ok(converted) if !converted.is_null())
        }
    }
}

/// Which of the two formats a step of the sniffer is about.
#[derive(Clone, Copy)]
enum Kind {
    Date,
    Timestamp,
}

impl Kind {
    fn ty(self) -> LogicalType {
        match self {
            Kind::Date => LogicalType::Date,
            Kind::Timestamp => LogicalType::Timestamp,
        }
    }

    /// The pin's `format_template_candidates`, with `-` where the separator goes.
    fn templates(self) -> &'static [&'static str] {
        match self {
            Kind::Date => &["%m-%d-%Y", "%m-%d-%y", "%d-%m-%Y", "%d-%m-%y", "%Y-%m-%d", "%y-%m-%d"],
            Kind::Timestamp => &[
                "%Y-%m-%d %H:%M:%S.%f",
                "%m-%d-%Y %I:%M:%S %p",
                "%m-%d-%y %I:%M:%S %p",
                "%d-%m-%Y %H:%M:%S",
                "%d-%m-%y %H:%M:%S",
                "%Y-%m-%d %H:%M:%S",
                "%y-%m-%d %H:%M:%S",
            ],
        }
    }
}

impl Clocks {
    fn of(&self, kind: Kind) -> Option<&Written> {
        match kind {
            Kind::Date => self.date.as_ref(),
            Kind::Timestamp => self.timestamp.as_ref(),
        }
    }

    fn slot(&mut self, kind: Kind) -> &mut Option<Written> {
        match kind {
            Kind::Date => &mut self.date,
            Kind::Timestamp => &mut self.timestamp,
        }
    }
}

/// The formats still in the running for one of the two types, the pin's `DateTimestampSniffing`.
#[derive(Default)]
struct Candidates {
    made: bool,
    /// The ones not yet ruled out, tried from the back.
    formats: Vec<String>,
    /// Whether one of them has read a value since the list was made.
    matched: bool,
    /// How long the list was when the last of the two was made, the pin's
    /// `original_format_candidates`, which tells a list nothing has been ruled out of yet.
    original: usize,
}

/// The pin's sniffer state across the columns of one sample. The formats are the sniffer's and not
/// a column's, so a second column of dates is held to the format the first one settled on.
struct Sniffer<'a> {
    given: &'a Clocks,
    /// The format each type is read with right now.
    current: Clocks,
    dates: Candidates,
    timestamps: Candidates,
    /// Whether a column so far came out a `DATE`, or a `TIMESTAMP`, after which the formats for
    /// that type are not looked for again.
    has_date: bool,
    has_timestamp: bool,
    /// Whether formats are looked for at all, which they are not when nothing is sniffed.
    detect: bool,
}

impl<'a> Sniffer<'a> {
    fn new(given: &'a Clocks, detect: bool) -> Self {
        Self {
            detect,
            given,
            current: given.clone(),
            dates: Candidates::default(),
            timestamps: Candidates::default(),
            has_date: false,
            has_timestamp: false,
        }
    }

    fn candidates(&mut self, kind: Kind) -> &mut Candidates {
        match kind {
            Kind::Date => &mut self.dates,
            Kind::Timestamp => &mut self.timestamps,
        }
    }

    /// The type of one column, the pin's `SniffTypes` for it, walking down `ladder`.
    fn column(&mut self, values: &[Option<&str>], ladder: &[LogicalType]) -> LogicalType {
        // `None` until the first value that is not a null, then the rung the column is on, with
        // the end of the ladder for `VARCHAR`.
        let mut rung: Option<usize> = None;
        for text in values.iter().copied().flatten() {
            let top = rung.and_then(|at| ladder.get(at));
            let separator = numeric_date(text).filter(|_| self.detect);
            let mut at = rung.unwrap_or(0);
            while let Some(ty) = ladder.get(at) {
                if let Some(separator) = &separator {
                    match ty {
                        LogicalType::Date if !self.has_date => {
                            self.detect(Kind::Date, separator, text);
                        }
                        LogicalType::Timestamp if !self.has_timestamp => {
                            self.detect(Kind::Timestamp, separator, text);
                        }
                        _ => {}
                    }
                }
                if fits_in(text, ty, &self.current) {
                    break;
                }
                // A column that settled on one of these drops straight to text when a value does
                // not fit, rather than trying the numbers under it. A timestamp still has the one
                // with a zone under it to try, when that is next.
                let zoned = || ladder.get(rung.unwrap_or(0) + 1) == Some(&LogicalType::TimestampTz);
                if top.is_some_and(|top| match top {
                    LogicalType::Boolean
                    | LogicalType::Time
                    | LogicalType::Date
                    | LogicalType::TimestampTz => true,
                    LogicalType::Timestamp => !zoned(),
                    _ => false,
                }) {
                    at = ladder.len();
                    break;
                }
                at += 1;
            }
            rung = Some(at);
        }
        let ty = rung.and_then(|at| ladder.get(at).cloned()).unwrap_or(LogicalType::Varchar);
        match ty {
            LogicalType::Date => self.has_date = true,
            LogicalType::Timestamp => self.has_timestamp = true,
            _ => {}
        }
        ty
    }

    /// Settles which format `text` is read with, the pin's `DetectDateAndTimeStampFormats`.
    ///
    /// The list is made the first time a value that looks like a date comes along, from the
    /// templates with its separator in them, less the ISO one the cast reads anyway. It is tried
    /// from the back and a format that does not read the value is dropped. If none does, the list
    /// is put back the way it was, unless nothing had been ruled out and nothing had matched yet,
    /// in which case it was a value of some other type and the list is thrown away, to be made
    /// again.
    fn detect(&mut self, kind: Kind, separator: &str, text: &str) {
        if !matches!(separator, "-" | "." | "/" | " ") {
            return;
        }
        let user = self.given.of(kind).map(|user| user.text.clone());
        if !self.candidates(kind).made {
            let formats = match &user {
                Some(user) => vec![user.clone()],
                None => kind
                    .templates()
                    .iter()
                    .map(|template| template.replace('-', separator))
                    .filter(|format| !format.contains("%Y-%m-%d"))
                    .collect(),
            };
            let candidates = self.candidates(kind);
            candidates.made = true;
            candidates.formats = formats;
            self.dates.original = self.dates.formats.len();
            self.timestamps.original = self.timestamps.formats.len();
        }
        let last = self.candidates(kind).formats.last().cloned();
        self.set(kind, last.as_deref());
        let saved = self.candidates(kind).formats.clone();
        let initial = saved.len() == self.candidates(kind).original;
        if user.is_none() {
            while !self.candidates(kind).formats.is_empty() {
                let current = self.current.of(kind);
                if current.is_some_and(|format| fits_strictly(&format.format, text)) {
                    self.candidates(kind).matched = true;
                    break;
                }
                self.candidates(kind).formats.pop();
                let last = self.candidates(kind).formats.last().cloned();
                self.set(kind, last.as_deref());
            }
        }
        if !self.candidates(kind).formats.is_empty() || saved.is_empty() {
            return;
        }
        if initial && !self.candidates(kind).matched {
            let original = self.candidates(kind).original;
            *self.candidates(kind) = Candidates { original, ..Candidates::default() };
            self.set(kind, None);
            return;
        }
        let last = saved.last().cloned();
        self.candidates(kind).formats = saved;
        self.set(kind, last.as_deref());
    }

    fn set(&mut self, kind: Kind, format: Option<&str>) {
        *self.current.slot(kind) = format.and_then(|format| Written::parse(format).ok());
    }
}

/// The separator between the numbers of a value that starts like a date, the pin's
/// `StartsWithNumericDate`: a number, some text, a number, the same text again and a third number,
/// after any spaces. A `%` in the separator is doubled, as a format writes one.
fn numeric_date(text: &str) -> Option<String> {
    let text = text.trim_start_matches(|c: char| c.is_ascii_whitespace()).as_bytes();
    let digit = |byte: &u8| byte.is_ascii_digit();
    if !text.first().is_some_and(digit) {
        return None;
    }
    let literal1 = text.iter().position(|byte| !digit(byte))?;
    let field2 = literal1 + text[literal1..].iter().position(digit)?;
    let literal2 = field2 + text[field2..].iter().position(|byte| !digit(byte))?;
    let field3 = literal2 + text[literal2..].iter().position(digit)?;
    let separator = &text[literal1..field2];
    if text[literal2..field3] != *separator {
        return None;
    }
    Some(String::from_utf8_lossy(separator).replace('%', "%%"))
}

/// Whether a written day carries a time on the end of it.
///
/// The third rule that is not the cast's. `CAST('2013-07-15 10:00:00' AS DATE)` is a date upstream,
/// the time is read and thrown away, so the two rungs do overlap and the sniffer has to tell them
/// apart itself or every timestamp column would come back as a `DATE`.
fn timed(text: &str) -> bool {
    text.trim().contains([' ', 'T'])
}

/// Whether a value is nothing but a clock.
///
/// The fourth rule that is not the cast's, and the same sort of rule as `timed`. The cast to `TIME`
/// takes a date in front and any amount of rubbish behind, so it would take a whole timestamp
/// column and every value in it would lose its day. A column of `12:34:56 UTC` is a `VARCHAR` to
/// the binary, which is what this refuses it for.
///
/// The shape is the one the pin's sniffer holds a value to, which is `Time::TryConvertTime` in its
/// strict mode: hours, a colon, a minute of one or two digits, and then either the end, where the
/// minute has to have two, or a colon and a second of one or two digits, a fraction after that if
/// there is one, and nothing but spaces around it. So `12:00` and `12:0:5` are clocks, and `12:`,
/// `12:0` and `12:0:` are not, although the cast takes all five.
fn clock(text: &str) -> bool {
    let text = text.trim().as_bytes();
    let digits = |from: usize, most: usize| {
        text[from..].iter().take(most).take_while(|byte| byte.is_ascii_digit()).count()
    };
    let hours = digits(0, 10);
    if hours == 0 || hours > 9 || text.get(hours) != Some(&b':') {
        return false;
    }
    let minute = digits(hours + 1, 2);
    let at = hours + 1 + minute;
    if minute == 0 {
        return false;
    }
    if at == text.len() {
        return minute == 2;
    }
    let second = if text[at] == b':' { digits(at + 1, 2) } else { 0 };
    if second == 0 {
        return false;
    }
    let at = at + 1 + second;
    match text.get(at) {
        None => true,
        Some(b'.') => text[at + 1..].iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

/// The spellings DuckDB's sniffer reads as a boolean.
///
/// Not `1` and `0`, although the cast takes both, because a column of ones and zeroes is a column of
/// numbers far more often than it is a column of flags and the binary agrees. Not `y` and `n`
/// either, and not `on` and `off`, both of which were tried against it.
fn is_boolean(text: &str) -> bool {
    ["true", "false", "t", "f", "yes", "no"]
        .iter()
        .any(|spelling| text.trim().eq_ignore_ascii_case(spelling))
}

/// Whether a number written like this is a number to the sniffer.
///
/// Three of the rules that are not the cast's. A leading `+` is refused, and so is a leading zero
/// with another digit behind it, which is how a column of `007` stays a column of `007` rather than
/// becoming a column of sevens. The sign is looked past for neither of them, because the binary does
/// not look past it either: `-007` really is a `BIGINT` there and this reproduces that rather than
/// tidying it up. A separator is refused on both rungs, so a column of `1_000` is a `VARCHAR` column
/// although `CAST('1_000' AS BIGINT)` is a thousand.
fn numeric(text: &str) -> bool {
    let text = text.trim();
    if text.contains('_') {
        return false;
    }
    let mut bytes = text.bytes();
    match bytes.next() {
        Some(b'+') => false,
        Some(b'0') => !matches!(bytes.next(), Some(byte) if byte.is_ascii_digit()),
        _ => true,
    }
}

/// Whether a number is one the sniffer puts on the `BIGNUM` rung, the pin's `CanYouCastBignum`:
/// digits with an optional minus in front and spaces around, no `+`, and no zero in front of
/// another digit.
fn bignum(text: &str) -> bool {
    let text = text.trim();
    let digits = text.strip_prefix('-').unwrap_or(text).as_bytes();
    !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && !(digits.len() > 1 && digits[0] == b'0')
}

/// Whether a whole number written like this is one to the sniffer.
///
/// The sixth rule that is not the cast's, and the one the rest of #369 turned up. The cast reads
/// `'1.5'` as two and `'1e3'` as a thousand, and upstream sniffs a column of either as `DOUBLE`, so
/// the rung above `DOUBLE` has to refuse a point and an exponent itself or every column of written
/// decimals would come back rounded. A radix is looked at first, because `0x1e` is thirty and the
/// `e` in the middle of it is a digit.
fn whole(text: &str) -> bool {
    let text = text.trim();
    matches!(text.get(..2), Some("0x" | "0X" | "0b" | "0B")) || !text.contains(['.', 'e', 'E'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(values: &[&str]) -> LogicalType {
        let values: Vec<Option<&str>> = values.iter().map(|text| Some(*text)).collect();
        column(&values)
    }

    #[test]
    fn each_rung_of_the_ladder_is_the_type_duckdb_sniffs_for_it() {
        assert_eq!(of(&["true", "false"]), LogicalType::Boolean);
        assert_eq!(of(&["1", "2"]), LogicalType::BigInt);
        assert_eq!(of(&["1.5", "2"]), LogicalType::Double);
        assert_eq!(of(&["2020-01-02", "2021-03-04"]), LogicalType::Date);
        assert_eq!(of(&["2020-01-02 03:04:05"]), LogicalType::Timestamp);
        assert_eq!(of(&["1", "x"]), LogicalType::Varchar);
    }

    /// The spellings the cast learned in #369 and what the sniffer does with each of them, all of
    /// it read off the pinned binary one file at a time. A radix stays on the `BIGINT` rung, a
    /// point and an exponent drop to `DOUBLE`, and a separator drops all the way.
    #[test]
    fn a_column_of_numbers_written_the_other_ways_lands_on_the_rung_duckdb_puts_it_on() {
        assert_eq!(of(&["0x10"]), LogicalType::BigInt);
        assert_eq!(of(&["0X10"]), LogicalType::BigInt);
        assert_eq!(of(&["0x1e"]), LogicalType::BigInt);
        assert_eq!(of(&["0b101"]), LogicalType::BigInt);
        assert_eq!(of(&["1.5"]), LogicalType::Double);
        assert_eq!(of(&["1."]), LogicalType::Double);
        assert_eq!(of(&[".5"]), LogicalType::Double);
        assert_eq!(of(&["1e3"]), LogicalType::Double);
        assert_eq!(of(&["1E3"]), LogicalType::Double);
        assert_eq!(of(&["1e-3"]), LogicalType::Double);
        assert_eq!(of(&["1e18"]), LogicalType::Double);
        assert_eq!(of(&["1_000"]), LogicalType::Varchar);
        assert_eq!(of(&["1_000", "1"]), LogicalType::Varchar);
        assert_eq!(of(&["1.5", "1_0.5"]), LogicalType::Varchar);
        assert_eq!(of(&["1e"]), LogicalType::Varchar);
    }

    /// The `TIME` rung, and the two values next to it that the binary leaves alone although the
    /// cast underneath takes both.
    #[test]
    fn a_column_of_clocks_is_a_time_and_a_column_of_anything_else_is_not() {
        assert_eq!(of(&["03:04:05", "12:34:56"]), LogicalType::Time);
        assert_eq!(of(&["12:34"]), LogicalType::Time);
        assert_eq!(of(&["12:34:56.5"]), LogicalType::Time);
        assert_eq!(of(&["12:34:56 UTC"]), LogicalType::Varchar);
        // The pin's sniffer, measured on `v2.0.0-dev84237`, which casts all of these.
        assert_eq!(of(&["12:0"]), LogicalType::Varchar);
        assert_eq!(of(&["12:"]), LogicalType::Varchar);
        assert_eq!(of(&["12:0:"]), LogicalType::Varchar);
        assert_eq!(of(&["12:0:5", " 7:05:06.25 "]), LogicalType::Time);
        assert_eq!(of(&["2020-01-02 03:04:05"]), LogicalType::Timestamp);
        for taken in ["12:34:56 UTC", "2020-01-02 03:04:05"] {
            let value = Value::Varchar(taken.into());
            assert!(
                cast_value(&value, &LogicalType::Time, true).is_ok_and(|time| !time.is_null()),
                "{taken}: the cast takes it, which is why the rung has a rule of its own"
            );
        }
    }

    /// The `DATE` rung would swallow this column otherwise, because the cast under it takes a time
    /// on the end of a day and throws it away, so the column would be declared a `DATE` and every
    /// value in it would lose its time.
    #[test]
    fn a_column_of_timestamps_does_not_stop_at_the_date_rung() {
        assert_eq!(of(&["2020-01-02 03:04:05", "2020-01-03 00:00:00"]), LogicalType::Timestamp);
        assert_eq!(of(&["2020-01-02T03:04:05"]), LogicalType::Timestamp);
        // A column on the `DATE` rung falls to text at a value that is not a day, rather than to
        // the `TIMESTAMP` rung under it, where a timestamp column takes a bare day.
        assert_eq!(of(&["2020-01-02", "2020-01-03 03:04:05"]), LogicalType::Varchar);
        assert_eq!(of(&["2020-01-03 03:04:05", "2020-01-02"]), LogicalType::Timestamp);
        let date = Value::Varchar("2020-01-02 03:04:05".into());
        assert!(
            cast_value(&date, &LogicalType::Date, true).is_ok_and(|value| !value.is_null()),
            "the cast still takes it, which is why the rung has a rule of its own"
        );
    }

    fn sniffed(columns: &[&[&str]]) -> (Vec<LogicalType>, Option<String>, Option<String>) {
        let columns: Vec<Vec<Option<&str>>> = columns
            .iter()
            .map(|values| {
                values.iter().map(|text| Some(*text).filter(|text| !text.is_empty())).collect()
            })
            .collect();
        let (types, clocks) = sniff(&columns, None, &Clocks::default(), true, &LADDER);
        let text = |written: Option<Written>| written.map(|written| written.text);
        (types, text(clocks.date), text(clocks.timestamp))
    }

    fn dated(format: &str) -> (Vec<LogicalType>, Option<String>, Option<String>) {
        (vec![LogicalType::Date], Some(format.to_string()), None)
    }

    /// Each of these is a file read on `v2.0.0-dev84237`, one column to a file unless it says.
    #[test]
    fn a_day_written_the_other_ways_round_is_read_with_the_format_that_fits() {
        assert_eq!(sniffed(&[&["05/06/2019", "13/01/2020"]]), dated("%d/%m/%Y"));
        assert_eq!(sniffed(&[&["01/13/2020", "05/06/2019"]]), dated("%m/%d/%Y"));
        // The format moves to the one that reads the second value, and the first no longer reads,
        // which is an error when the file is read rather than here.
        assert_eq!(sniffed(&[&["13/01/2020", "01/13/2020"]]), dated("%m/%d/%Y"));
        assert_eq!(sniffed(&[&["05-06-2019"]]), dated("%d-%m-%Y"));
        assert_eq!(sniffed(&[&["05.06.2019"]]), dated("%d.%m.%Y"));
        assert_eq!(sniffed(&[&["2019/06/05"]]), dated("%Y/%m/%d"));
        assert_eq!(sniffed(&[&["05/06/19"]]), dated("%y/%m/%d"));
        assert_eq!(sniffed(&[&["05/06/2019", "", "13/01/2020"]]), dated("%d/%m/%Y"));
        // The second column is held to the first one's format and does not fit it.
        let (types, date, _) = sniffed(&[&["13/01/2020"], &["01/13/2020"]]);
        assert_eq!(types, [LogicalType::Date, LogicalType::Varchar]);
        assert_eq!(date.as_deref(), Some("%d/%m/%Y"));
        // A day in ISO form is the cast's, and a column after one does not look for a format.
        let (types, date, _) = sniffed(&[&["2019-01-01"], &["05/06/2019"]]);
        assert_eq!(types, [LogicalType::Date, LogicalType::Varchar]);
        assert_eq!(date, None);
        assert_eq!(sniffed(&[&["05/06/2019", "2019-01-01"]]).0, [LogicalType::Varchar]);
        assert_eq!(sniffed(&[&["foo"]]), (vec![LogicalType::Varchar], None, None));
    }

    #[test]
    fn a_timestamp_written_the_other_ways_round_is_read_with_the_format_that_fits() {
        let stamped = |format: &str| (vec![LogicalType::Timestamp], None, Some(format.to_string()));
        assert_eq!(sniffed(&[&["05/06/2019 10:11:12"]]), stamped("%d/%m/%Y %H:%M:%S"));
        assert_eq!(sniffed(&[&["12/31/2019 10:11:12 PM"]]), stamped("%m/%d/%Y %I:%M:%S %p"));
        assert_eq!(
            sniffed(&[&["2019-06-05 10:11:12"]]),
            (vec![LogicalType::Timestamp], None, None)
        );
        let (types, date, timestamp) = sniffed(&[&["07/08/2019"], &["07/08/2019 10:11:12"]]);
        assert_eq!(types, [LogicalType::Date, LogicalType::Timestamp]);
        assert_eq!(date.as_deref(), Some("%d/%m/%Y"));
        assert_eq!(timestamp.as_deref(), Some("%d/%m/%Y %H:%M:%S"));
    }

    #[test]
    fn a_format_the_call_gives_is_the_only_one_tried() {
        let given =
            Clocks { date: Some(Written::parse("%m/%d/%Y").expect("parses")), timestamp: None };
        let columns = [vec![Some("05/06/2019"), Some("13/01/2020")], vec![Some("12/31/2019")]];
        let (types, clocks) = sniff(&columns, None, &given, true, &LADDER);
        assert_eq!(types, [LogicalType::Varchar, LogicalType::Date]);
        assert_eq!(clocks.date.map(|written| written.text).as_deref(), Some("%m/%d/%Y"));
        let (types, clocks) = sniff(&[vec![Some("1")]], None, &given, true, &LADDER);
        assert_eq!(types, [LogicalType::BigInt]);
        assert!(clocks.date.is_some(), "a format the call set is kept without a date column");
        assert!(Written::parse("%Q").is_err());
    }

    #[test]
    fn a_column_the_call_typed_keeps_its_type_and_still_settles_the_format() {
        let columns = [vec![Some("05/06/2019"), Some("13/01/2020")], vec![Some("x")]];
        let set = [LogicalType::Date, LogicalType::Varchar];
        let (types, clocks) = sniff(&columns, Some(&set), &Clocks::default(), true, &LADDER);
        assert_eq!(types, set);
        assert_eq!(clocks.date.map(|written| written.text).as_deref(), Some("%d/%m/%Y"));
        let (types, clocks) = sniff(&columns[..1], None, &Clocks::default(), false, &LADDER);
        assert_eq!(types, [LogicalType::Varchar]);
        assert!(clocks.date.is_none());
    }

    #[test]
    fn a_value_that_starts_like_a_date_gives_its_separator() {
        assert_eq!(numeric_date("05/06/2019").as_deref(), Some("/"));
        assert_eq!(numeric_date("  1 2 3").as_deref(), Some(" "));
        assert_eq!(numeric_date("1%2%3").as_deref(), Some("%%"));
        assert_eq!(numeric_date("1/2-3"), None);
        assert_eq!(numeric_date("1.5"), None);
        assert_eq!(numeric_date("x1/2/3"), None);
        assert_eq!(numeric_date("1/2/"), None);
    }

    #[test]
    fn a_column_on_a_rung_with_a_rule_of_its_own_falls_to_text_and_not_down_a_rung() {
        assert_eq!(of(&["true", "1"]), LogicalType::Varchar);
        assert_eq!(of(&["1", "true"]), LogicalType::Varchar);
        assert_eq!(of(&["03:04:05", "1"]), LogicalType::Varchar);
        assert_eq!(of(&["1", "1.5"]), LogicalType::Double);
    }

    #[test]
    fn a_column_of_nothing_but_nulls_is_a_varchar() {
        assert_eq!(column(&[None, None]), LogicalType::Varchar);
        assert_eq!(column(&[]), LogicalType::Varchar);
    }

    #[test]
    fn a_null_in_a_column_does_not_change_what_the_rest_of_it_is() {
        assert_eq!(column(&[Some("1"), None, Some("2")]), LogicalType::BigInt);
    }

    #[test]
    fn ones_and_zeroes_are_numbers_rather_than_flags() {
        // Measured. `CAST('1' AS BOOLEAN)` is true, so a ladder that asked the cast would call this
        // column a boolean, and duckdb v1.4.1 calls it a BIGINT.
        assert_eq!(of(&["0", "1"]), LogicalType::BigInt);
    }

    #[test]
    fn the_boolean_spellings_are_the_six_the_binary_takes_and_no_more() {
        for yes in ["true", "TRUE", "True", "t", "T", "yes", "Yes"] {
            assert_eq!(of(&[yes]), LogicalType::Boolean, "{yes}");
        }
        for no in ["on", "off", "y", "n"] {
            assert_eq!(of(&[no]), LogicalType::Varchar, "{no}");
        }
    }

    #[test]
    fn a_zero_padded_number_stays_the_text_it_was_written_as() {
        assert_eq!(of(&["007", "008"]), LogicalType::Varchar);
        assert_eq!(of(&["00"]), LogicalType::Varchar);
        // And the two that go the other way, both measured against the binary.
        assert_eq!(of(&["0"]), LogicalType::BigInt);
        assert_eq!(of(&["-007"]), LogicalType::BigInt);
    }

    #[test]
    fn a_leading_plus_is_not_a_number_to_the_sniffer() {
        assert_eq!(of(&["+1"]), LogicalType::Varchar);
    }

    #[test]
    fn space_around_a_number_does_not_stop_it_being_one() {
        assert_eq!(of(&[" 1", "2 "]), LogicalType::BigInt);
    }

    #[test]
    fn a_whole_number_too_big_for_a_bigint_widens_to_a_hugeint_and_then_a_bignum() {
        assert_eq!(of(&["99999999999999999999"]), LogicalType::HugeInt);
        let long = "1".repeat(42);
        assert_eq!(of(&[long.as_str()]), LogicalType::BigNum);
        assert_eq!(of(&["1", long.as_str()]), LogicalType::BigNum);
        assert_eq!(of(&[long.as_str(), "1.5"]), LogicalType::Double);
        let padded = format!("0{long}");
        assert_eq!(of(&[padded.as_str()]), LogicalType::Varchar);
        assert!(bignum(" -12 ") && !bignum("+12") && !bignum("-") && !bignum("1_0"));
    }

    #[test]
    fn a_timestamp_with_an_offset_is_one_with_a_time_zone() {
        assert_eq!(of(&["2020-01-02 03:04:05+02"]), LogicalType::TimestampTz);
        assert_eq!(of(&["2020-01-02 03:04:05Z"]), LogicalType::TimestampTz);
        assert_eq!(of(&["2020-01-02 03:04:05 UTC"]), LogicalType::Timestamp);
        // A plain one first, then one with an offset, moves the column down a rung.
        let mixed = ["2020-01-02 03:04:05", "2020-01-02 03:04:05+02"];
        assert_eq!(of(&mixed), LogicalType::TimestampTz);
        // A day first goes to text, as it always did, since a date falls straight there.
        assert_eq!(of(&["2020-01-02", "2020-01-02 03:04:05+02"]), LogicalType::Varchar);
        assert_eq!(of(&["2020-01-02 03:04:05+02", "x"]), LogicalType::Varchar);
        assert_eq!(of(&["2020-01-02 03:04:05+02", "2020-01-02"]), LogicalType::TimestampTz);
        // With no zoned rung under it a timestamp falls straight to text too.
        let ladder = [LogicalType::Timestamp, LogicalType::BigInt];
        let column = vec![Some("2020-01-02 03:04:05"), Some("1")];
        let (types, _) = sniff(&[column], None, &Clocks::default(), true, &ladder);
        assert_eq!(types, [LogicalType::Varchar]);
    }

    #[test]
    fn a_call_sets_the_ladder_and_it_is_put_in_the_pin_order() {
        let set = ladder(&[
            LogicalType::Varchar,
            LogicalType::Double,
            LogicalType::TinyInt,
            LogicalType::Boolean,
            LogicalType::Null,
            LogicalType::Float,
        ]);
        let want =
            [LogicalType::Boolean, LogicalType::TinyInt, LogicalType::Float, LogicalType::Double];
        assert_eq!(set, want);
        assert_eq!(ladder(&LADDER), LADDER);
        assert_eq!(specificity(&LogicalType::UTinyInt), None);
        let column = vec![Some("300"), Some("1")];
        let tiny = ladder(&[LogicalType::TinyInt, LogicalType::Float]);
        let (types, _) = sniff(&[column], None, &Clocks::default(), true, &tiny);
        assert_eq!(types, [LogicalType::Float]);
    }
}
