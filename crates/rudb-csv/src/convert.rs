//! Fields into columns, a column at a time.
//!
//! A chunk arrives from [`crate::scan::records`] as ranges into the buffer, and each projected
//! column is built from them straight into the run of values its type is stored as. The types the
//! sniffer produces and a few more have a parser here that reads the bytes where they are. Anything
//! such a parser does not take, and every other type, goes through `cast_value` from `VARCHAR` one
//! cell at a time, which is what every cell used to go through. So the parsers only ever have to
//! agree with the cast about the values they accept, and a value they are not sure of is the
//! cast's to accept or refuse in its own words.
//!
//! What each parser takes was read off the cast rather than off DuckDB. A whole number is an
//! optional `+` or `-` and up to eighteen digits, leading zeros and all, which the cast reads the
//! same way; spaces, underscores, a point, an exponent and the `0x` spellings are left to it. A
//! double is the characters of a plain decimal number handed to the same `str::parse` the cast ends
//! in, with `inf`, `nan` and anything with a space or an underscore left to the cast. A boolean is
//! one of the ten spellings the cast knows, in any case, without spaces. A date is exactly
//! `YYYY-MM-DD`.

use rudb_common::{Error, LogicalType, Result, Value, days_from_civil};
use rudb_kernels::cast_value;
use rudb_vector::{Buffer, Data, INLINE_LIMIT, StringColumn, Validity, Vector};

use crate::dialect::Dialect;
use crate::scan::{Records, Span};

/// One chunk's fields and the buffer their ranges point into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cells<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) records: &'a Records,
    pub(crate) dialect: Dialect,
}

impl Cells<'_> {
    /// Field `column` of row `row`, or `None` for a null, which is an empty field or one the row
    /// is too short to have.
    fn at(&self, row: usize, column: usize) -> Option<Span> {
        self.records.field(row, column).filter(|span| !span.is_empty())
    }
}

/// Column `column` of every row in `cells`, as `ty`.
///
/// `refuse` makes the error for a value that does not convert, given its text and its row within
/// the chunk, and is only called for the first one. The rows are converted in order, so that one
/// is the one the reader used to report.
pub(crate) fn column(
    cells: &Cells<'_>,
    column: usize,
    ty: &LogicalType,
    refuse: &dyn Fn(&str, usize) -> Error,
) -> Result<Vector> {
    match ty {
        LogicalType::Varchar => Ok(text(cells, column, ty)),
        LogicalType::Boolean => fixed(cells, column, ty, refuse, truth, bool_of, Data::Bool),
        LogicalType::TinyInt => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| i8::try_from(x).ok()),
            |value| if let Value::TinyInt(x) = value { Some(*x) } else { None },
            Data::Int8,
        ),
        LogicalType::SmallInt => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| i16::try_from(x).ok()),
            |value| if let Value::SmallInt(x) = value { Some(*x) } else { None },
            Data::Int16,
        ),
        LogicalType::Integer => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| i32::try_from(x).ok()),
            |value| if let Value::Integer(x) = value { Some(*x) } else { None },
            Data::Int32,
        ),
        LogicalType::BigInt => fixed(
            cells,
            column,
            ty,
            refuse,
            whole,
            |value| if let Value::BigInt(x) = value { Some(*x) } else { None },
            Data::Int64,
        ),
        LogicalType::UTinyInt => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| u8::try_from(x).ok()),
            |value| if let Value::UTinyInt(x) = value { Some(*x) } else { None },
            Data::UInt8,
        ),
        LogicalType::USmallInt => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| u16::try_from(x).ok()),
            |value| if let Value::USmallInt(x) = value { Some(*x) } else { None },
            Data::UInt16,
        ),
        LogicalType::UInteger => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| u32::try_from(x).ok()),
            |value| if let Value::UInteger(x) = value { Some(*x) } else { None },
            Data::UInt32,
        ),
        LogicalType::UBigInt => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| whole(raw).and_then(|x| u64::try_from(x).ok()),
            |value| if let Value::UBigInt(x) = value { Some(*x) } else { None },
            Data::UInt64,
        ),
        LogicalType::Double => fixed(
            cells,
            column,
            ty,
            refuse,
            real,
            |value| if let Value::Double(x) = value { Some(*x) } else { None },
            Data::Float64,
        ),
        LogicalType::Float => fixed(
            cells,
            column,
            ty,
            refuse,
            |raw| real(raw).map(narrow),
            |value| if let Value::Float(x) = value { Some(*x) } else { None },
            Data::Float32,
        ),
        LogicalType::Date => fixed(
            cells,
            column,
            ty,
            refuse,
            day,
            |value| if let Value::Date(x) = value { Some(*x) } else { None },
            Data::Int32,
        ),
        _ => values(cells, column, ty, refuse),
    }
}

/// A column of a fixed width type, parsed in place where `fast` can and cast where it cannot.
///
/// `native` takes the value the cast produced back out of its `Value`, which for a cast to the
/// column's own type is always the variant it names.
fn fixed<T: Copy + Default>(
    cells: &Cells<'_>,
    column: usize,
    ty: &LogicalType,
    refuse: &dyn Fn(&str, usize) -> Error,
    fast: impl Fn(&[u8]) -> Option<T>,
    native: impl Fn(&Value) -> Option<T>,
    wrap: impl FnOnce(Buffer<T>) -> Data,
) -> Result<Vector> {
    let rows = cells.records.len();
    let mut out = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let Some(span) = cells.at(row, column) else {
            out.push(T::default());
            valid.push(false);
            continue;
        };
        let parsed = if span.escaped { None } else { fast(span.raw(cells.bytes)) };
        let value = match parsed {
            Some(value) => value,
            None => {
                let cast = cast(cells, span, ty, row, refuse)?;
                native(&cast).ok_or_else(|| {
                    Error::internal(format!("{cast:?} does not belong in a {ty} column"))
                })?
            }
        };
        out.push(value);
        valid.push(true);
    }
    Ok(Vector::flat(ty.clone(), wrap(Buffer::from_vec(out)))?
        .with_validity(Validity::from_run(&valid)))
}

/// A `VARCHAR` column, copied once from the buffer into the column's arena.
///
/// A field that is valid UTF-8 and has no escape in it goes in as the bytes it is. The rest go in
/// as the text [`Span::text`] makes of them, which is the text the cell used to be.
fn text(cells: &Cells<'_>, column: usize, ty: &LogicalType) -> Vector {
    let rows = cells.records.len();
    let mut strings = StringColumn::with_capacity(rows);
    let long: usize = (0..rows)
        .filter_map(|row| cells.at(row, column))
        .map(|span| span.end - span.start)
        .filter(|&len| len > INLINE_LIMIT)
        .sum();
    strings.reserve_bytes(long);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let Some(span) = cells.at(row, column) else {
            strings.push("");
            valid.push(false);
            continue;
        };
        let raw = span.raw(cells.bytes);
        if !span.escaped && rudb_common::utf8::valid(raw) {
            strings.push_bytes(raw);
        } else {
            strings.push(&span.text(cells.bytes, cells.dialect));
        }
        valid.push(true);
    }
    Vector::flat(ty.clone(), Data::Varlen(strings))
        .expect("a VARCHAR vector holds strings")
        .with_validity(Validity::from_run(&valid))
}

/// A column of a type with no parser here, one `Value` at a time the way every column used to be.
fn values(
    cells: &Cells<'_>,
    column: usize,
    ty: &LogicalType,
    refuse: &dyn Fn(&str, usize) -> Error,
) -> Result<Vector> {
    let rows = cells.records.len();
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        out.push(match cells.at(row, column) {
            None => Value::Null,
            Some(span) => cast(cells, span, ty, row, refuse)?,
        });
    }
    Vector::from_values(ty.clone(), &out)
}

/// One cell through the cast from `VARCHAR`, with the reader's error in place of the cast's.
fn cast(
    cells: &Cells<'_>,
    span: Span,
    ty: &LogicalType,
    row: usize,
    refuse: &dyn Fn(&str, usize) -> Error,
) -> Result<Value> {
    let text = span.text(cells.bytes, cells.dialect);
    cast_value(&Value::Varchar(text.to_string()), ty, false).map_err(|_| refuse(&text, row))
}

/// A whole number written as an optional sign and up to eighteen digits.
///
/// Eighteen digits cannot overflow an `i64`, so there is no check in the loop, and a longer number
/// is the cast's, which is also where one too big for the column's type goes.
fn whole(raw: &[u8]) -> Option<i64> {
    let (negative, digits) = match raw {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, raw),
    };
    if digits.is_empty() || digits.len() > 18 {
        return None;
    }
    let mut value = 0i64;
    for &byte in digits {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value * 10 + i64::from(digit);
    }
    Some(if negative { -value } else { value })
}

/// A double written with nothing but digits, a point, signs and an exponent.
///
/// Those are the bytes on which the cast's own reading comes down to `str::parse` with nothing
/// trimmed and no separators taken out, so this is that call and gives the cast's answer. The
/// common case of a short plain decimal is worked out directly first, see [`short_decimal`].
fn real(raw: &[u8]) -> Option<f64> {
    if let Some(number) = short_decimal(raw) {
        return Some(number);
    }
    if raw.is_empty()
        || !raw
            .iter()
            .all(|&byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-' | b'e' | b'E'))
    {
        return None;
    }
    std::str::from_utf8(raw).ok()?.parse().ok()
}

/// The powers of ten a double holds exactly.
const EXACT: [f64; 16] =
    [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15];

/// A decimal of at most fifteen digits with no exponent, such as `17.00` or `-0.05`, which is most
/// of the doubles in a CSV file.
///
/// Fifteen digits are an integer below 2^53 and so exact as a double, ten to the fifteenth is exact
/// too, and a division of two exact doubles is rounded correctly, so the quotient is the double
/// nearest the decimal. That is Clinger's fast path, and `str::parse` gives the double nearest the
/// decimal as well, so the two agree without this having to do any of the work that makes parsing
/// in general hard.
fn short_decimal(raw: &[u8]) -> Option<f64> {
    let (negative, rest) = match raw {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, raw),
    };
    let mut mantissa = 0u64;
    let mut digits = 0usize;
    let mut scale = 0usize;
    let mut point = false;
    for &byte in rest {
        if byte == b'.' && !point {
            point = true;
            continue;
        }
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 || digits == 15 {
            return None;
        }
        mantissa = mantissa * 10 + u64::from(digit);
        digits += 1;
        scale += usize::from(point);
    }
    if digits == 0 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "fifteen digits are below 2^53 and exact")]
    let number = mantissa as f64 / EXACT[scale];
    Some(if negative { -number } else { number })
}

/// A double as a float.
///
/// The cast reads a double and narrows it, so this does the same rather than parsing a float
/// directly, which can round a number differently.
#[expect(clippy::cast_possible_truncation, reason = "narrowing is what a FLOAT is")]
const fn narrow(number: f64) -> f32 {
    number as f32
}

/// A boolean written as one of the spellings the cast takes, in any case and with no spaces.
fn truth(raw: &[u8]) -> Option<bool> {
    const TRUE: [&[u8]; 5] = [b"true", b"t", b"yes", b"y", b"1"];
    const FALSE: [&[u8]; 5] = [b"false", b"f", b"no", b"n", b"0"];
    if TRUE.iter().any(|spelling| raw.eq_ignore_ascii_case(spelling)) {
        Some(true)
    } else if FALSE.iter().any(|spelling| raw.eq_ignore_ascii_case(spelling)) {
        Some(false)
    } else {
        None
    }
}

/// The boolean in a value the cast produced.
fn bool_of(value: &Value) -> Option<bool> {
    if let Value::Boolean(x) = value { Some(*x) } else { None }
}

/// A date written exactly as `YYYY-MM-DD`, as days since the epoch.
///
/// A four digit year cannot reach the edges of the range a date is kept in, so the only checks are
/// the month and the day, and a day the month does not have is the cast's to refuse.
fn day(raw: &[u8]) -> Option<i32> {
    let &[y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = raw else { return None };
    let digit = |byte: u8| {
        let digit = byte.wrapping_sub(b'0');
        (digit <= 9).then_some(u32::from(digit))
    };
    let year = digit(y0)? * 1000 + digit(y1)? * 100 + digit(y2)? * 10 + digit(y3)?;
    let month = digit(m0)? * 10 + digit(m1)?;
    let day = digit(d0)? * 10 + digit(d1)?;
    let year = i32::try_from(year).ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// How many days a month has, in the proleptic Gregorian calendar the cast counts in.
const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast_text(text: &str, ty: &LogicalType) -> Option<Value> {
        cast_value(&Value::Varchar(text.to_string()), ty, false).ok()
    }

    /// Every text a parser here accepts, the cast accepts as the same value.
    ///
    /// The other direction is not a requirement, since a text a parser turns down goes to the cast.
    #[test]
    fn the_parsers_agree_with_the_cast_on_everything_they_accept() {
        let texts = [
            "0",
            "-0",
            "+0",
            "1",
            "-1",
            "+1",
            "007",
            "-007",
            "127",
            "128",
            "-128",
            "-129",
            "255",
            "256",
            "32767",
            "32768",
            "65535",
            "65536",
            "2147483647",
            "2147483648",
            "-2147483649",
            "4294967295",
            "4294967296",
            "999999999999999999",
            "-999999999999999999",
            "9223372036854775807",
            "-9223372036854775808",
            "9999999999999999999",
            "18446744073709551615",
            "99999999999999999999",
            "1.5",
            "-1.5",
            ".5",
            "5.",
            "1e3",
            "1E-3",
            "+1.5e+10",
            "1e400",
            "-1e400",
            "1e-400",
            "0.1",
            "3.4028236e38",
            "1..2",
            "1e",
            "e1",
            "-",
            "+",
            "--1",
            "+-1",
            " 1",
            "1 ",
            "1_000",
            "0x10",
            "inf",
            "nan",
            "true",
            "TRUE",
            "t",
            "T",
            "yes",
            "Y",
            "false",
            "F",
            "no",
            "N",
            "tru",
            "2020-01-01",
            "0000-01-01",
            "9999-12-31",
            "2020-02-29",
            "2019-02-29",
            "1900-02-29",
            "2000-02-29",
            "2020-13-01",
            "2020-00-01",
            "2020-01-00",
            "2020-04-31",
            "2020-1-01",
            "+2020-01-01",
            "2020-01-01 ",
            "abc",
            "",
        ];
        let types = [
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::UTinyInt,
            LogicalType::USmallInt,
            LogicalType::UInteger,
            LogicalType::UBigInt,
            LogicalType::Double,
            LogicalType::Float,
            LogicalType::Boolean,
            LogicalType::Date,
        ];
        for text in texts {
            let raw = text.as_bytes();
            for ty in &types {
                let fast = match ty {
                    LogicalType::TinyInt => {
                        whole(raw).and_then(|x| i8::try_from(x).ok()).map(Value::TinyInt)
                    }
                    LogicalType::SmallInt => {
                        whole(raw).and_then(|x| i16::try_from(x).ok()).map(Value::SmallInt)
                    }
                    LogicalType::Integer => {
                        whole(raw).and_then(|x| i32::try_from(x).ok()).map(Value::Integer)
                    }
                    LogicalType::BigInt => whole(raw).map(Value::BigInt),
                    LogicalType::UTinyInt => {
                        whole(raw).and_then(|x| u8::try_from(x).ok()).map(Value::UTinyInt)
                    }
                    LogicalType::USmallInt => {
                        whole(raw).and_then(|x| u16::try_from(x).ok()).map(Value::USmallInt)
                    }
                    LogicalType::UInteger => {
                        whole(raw).and_then(|x| u32::try_from(x).ok()).map(Value::UInteger)
                    }
                    LogicalType::UBigInt => {
                        whole(raw).and_then(|x| u64::try_from(x).ok()).map(Value::UBigInt)
                    }
                    LogicalType::Double => real(raw).map(Value::Double),
                    LogicalType::Float => real(raw).map(|x| Value::Float(narrow(x))),
                    LogicalType::Boolean => truth(raw).map(Value::Boolean),
                    LogicalType::Date => day(raw).map(Value::Date),
                    _ => unreachable!(),
                };
                let Some(fast) = fast else { continue };
                let slow = cast_text(text, ty);
                let same = match (&fast, &slow) {
                    (Value::Double(a), Some(Value::Double(b))) => a.to_bits() == b.to_bits(),
                    (Value::Float(a), Some(Value::Float(b))) => a.to_bits() == b.to_bits(),
                    (fast, Some(slow)) => fast == slow,
                    (_, None) => false,
                };
                assert!(same, "{text:?} as {ty}: parsed {fast:?}, cast {slow:?}");
            }
        }
    }

    /// The short decimal path gives the double `str::parse` gives, over a lot of short decimals.
    #[test]
    fn short_decimals_are_the_doubles_parse_reads() {
        let mut state = 0x853c_49e6_748f_ea9b_u64;
        for _ in 0..200_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let digits = 1 + (state % 15) as usize;
            let mantissa = (state >> 8) % 10u64.pow(digits as u32);
            let mut text = format!("{mantissa:0digits$}");
            let point = ((state >> 40) % (digits as u64 + 1)) as usize;
            text.insert(point, '.');
            if state & (1 << 60) != 0 {
                text.insert(0, '-');
            }
            let fast = short_decimal(text.as_bytes()).expect("a short decimal");
            let slow: f64 = text.parse().expect("parses");
            assert_eq!(fast.to_bits(), slow.to_bits(), "{text}");
        }
        for text in ["1234567890123456", "1.", ".5", ".", "-.", "1.2.3", "1e5", "+-1", ""] {
            if let Some(fast) = short_decimal(text.as_bytes()) {
                assert_eq!(
                    Some(fast.to_bits()),
                    text.parse::<f64>().ok().map(f64::to_bits),
                    "{text}"
                );
            }
        }
    }

    /// Every day of four digit years, which is every date the date parser takes.
    #[test]
    fn every_date_the_parser_reads_is_the_day_the_cast_reads() {
        for year in (0..=9999).step_by(7).chain([0, 1, 1600, 1900, 1970, 2000, 2024, 9999]) {
            for month in 1..=12 {
                for day_of in 1..=31 {
                    let text = format!("{year:04}-{month:02}-{day_of:02}");
                    let fast = day(text.as_bytes()).map(Value::Date);
                    let slow = cast_text(&text, &LogicalType::Date);
                    if let Some(fast) = fast {
                        assert_eq!(Some(fast), slow, "{text}");
                    } else {
                        assert!(slow.is_none(), "{text} is a date the parser turned down");
                    }
                }
            }
        }
    }
}
