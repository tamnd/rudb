//! Fields into columns, a block of rows at a time.
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
//! optional `+` or `-` and up to twenty digits, leading zeros and all, that fit in 64 bits, which
//! the cast reads the same way; spaces, underscores, a point, an exponent and the `0x` spellings
//! are left to it, and so is a number too long or too big. A double is the characters of a plain
//! decimal number handed to the same `str::parse` the cast ends in, with `inf`, `nan` and anything
//! with a space or an underscore left to the cast. A boolean is one of the ten spellings the cast
//! knows, in any case, without spaces. A date is exactly `YYYY-MM-DD`.

use std::ops::Range;

use rudb_common::{Error, LogicalType, Result, Value, days_from_civil};
use rudb_kernels::cast_value;
use rudb_vector::{Buffer, Data, INLINE_LIMIT, StringColumn, StringView, Validity, Vector};

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
    let rows = cells.records.len();
    let mut build = builders(cells, &[(column, ty)]).pop().expect("one builder");
    build.rows(cells, column, 0..rows, refuse)?;
    build.finish()
}

/// A column being built a block of rows at a time.
///
/// The reader converts a chunk a block of rows at a time and every projected column within a
/// block, rather than every row of one column and then the next, so that the block's ranges and
/// the bytes they point at are still in the cache for the second column. A chunk's ranges are a
/// megabyte and its bytes about as much again, and a column at a time over all of them was a cache
/// miss for most fields.
pub(crate) trait Build {
    /// Adds `rows` of column `column` of `cells`, which come after the rows already added.
    ///
    /// # Errors
    ///
    /// The one `refuse` makes for the first value in `rows` that does not convert.
    fn rows(
        &mut self,
        cells: &Cells<'_>,
        column: usize,
        rows: Range<usize>,
        refuse: &dyn Fn(&str, usize) -> Error,
    ) -> Result<()>;

    /// The column, with every row added so far.
    ///
    /// # Errors
    ///
    /// If a cast gave a value that does not belong in the column, which is a bug in the cast.
    fn finish(self: Box<Self>) -> Result<Vector>;
}

/// A builder for each of `columns`, a column of `cells` and its type, with room for every row.
///
/// A `VARCHAR` column's arena is taken at the size its long strings need, all in one allocation.
/// They are counted here, every text column in one pass along the rows, which reads the ranges in
/// the order they are laid out rather than a column's worth at a stride of a whole row.
pub(crate) fn builders(
    cells: &Cells<'_>,
    columns: &[(usize, &LogicalType)],
) -> Vec<Box<dyn Build>> {
    let text: Vec<usize> = columns
        .iter()
        .filter(|(_, ty)| **ty == LogicalType::Varchar)
        .map(|&(column, _)| column)
        .collect();
    let mut long = vec![0; text.len()];
    if !text.is_empty() {
        for row in 0..cells.records.len() {
            for (sum, &column) in long.iter_mut().zip(&text) {
                if let Some(span) = cells.at(row, column) {
                    if span.len() > INLINE_LIMIT {
                        *sum += span.len();
                    }
                }
            }
        }
    }
    let rows = cells.records.len();
    let mut long = long.into_iter();
    columns
        .iter()
        .map(|&(_, ty)| {
            let long = if *ty == LogicalType::Varchar { long.next().unwrap_or(0) } else { 0 };
            builder(ty, rows, long)
        })
        .collect()
}

/// A builder for a column of `ty`, with room for `rows` and, for text, `long` bytes of long strings.
fn builder(ty: &LogicalType, rows: usize, long: usize) -> Box<dyn Build> {
    match ty {
        LogicalType::Varchar => Box::new(Text {
            ty: ty.clone(),
            views: Vec::with_capacity(rows),
            arena: Vec::with_capacity(long),
            valid: Vec::with_capacity(rows),
        }),
        LogicalType::Boolean => fixed(ty, rows, truth, bool_of, Data::Bool),
        LogicalType::TinyInt => fixed(
            ty,
            rows,
            |raw| whole(raw).and_then(|x| i8::try_from(x).ok()),
            |value| if let Value::TinyInt(x) = value { Some(*x) } else { None },
            Data::Int8,
        ),
        LogicalType::SmallInt => fixed(
            ty,
            rows,
            |raw| whole(raw).and_then(|x| i16::try_from(x).ok()),
            |value| if let Value::SmallInt(x) = value { Some(*x) } else { None },
            Data::Int16,
        ),
        LogicalType::Integer => fixed(
            ty,
            rows,
            |raw| whole(raw).and_then(|x| i32::try_from(x).ok()),
            |value| if let Value::Integer(x) = value { Some(*x) } else { None },
            Data::Int32,
        ),
        LogicalType::BigInt => fixed(
            ty,
            rows,
            whole,
            |value| if let Value::BigInt(x) = value { Some(*x) } else { None },
            Data::Int64,
        ),
        LogicalType::UTinyInt => fixed(
            ty,
            rows,
            |raw| natural(raw).and_then(|x| u8::try_from(x).ok()),
            |value| if let Value::UTinyInt(x) = value { Some(*x) } else { None },
            Data::UInt8,
        ),
        LogicalType::USmallInt => fixed(
            ty,
            rows,
            |raw| natural(raw).and_then(|x| u16::try_from(x).ok()),
            |value| if let Value::USmallInt(x) = value { Some(*x) } else { None },
            Data::UInt16,
        ),
        LogicalType::UInteger => fixed(
            ty,
            rows,
            |raw| natural(raw).and_then(|x| u32::try_from(x).ok()),
            |value| if let Value::UInteger(x) = value { Some(*x) } else { None },
            Data::UInt32,
        ),
        LogicalType::UBigInt => fixed(
            ty,
            rows,
            natural,
            |value| if let Value::UBigInt(x) = value { Some(*x) } else { None },
            Data::UInt64,
        ),
        LogicalType::Double => fixed(
            ty,
            rows,
            real,
            |value| if let Value::Double(x) = value { Some(*x) } else { None },
            Data::Float64,
        ),
        LogicalType::Float => fixed(
            ty,
            rows,
            |raw| real(raw).map(narrow),
            |value| if let Value::Float(x) = value { Some(*x) } else { None },
            Data::Float32,
        ),
        LogicalType::Date => fixed(
            ty,
            rows,
            day,
            |value| if let Value::Date(x) = value { Some(*x) } else { None },
            Data::Int32,
        ),
        _ => Box::new(Values { ty: ty.clone(), values: Vec::with_capacity(rows) }),
    }
}

/// A builder for a column of a fixed width type. See [`Fixed`].
fn fixed<T, F, N>(
    ty: &LogicalType,
    rows: usize,
    fast: F,
    native: N,
    wrap: fn(Buffer<T>) -> Data,
) -> Box<dyn Build>
where
    T: Copy + Default + 'static,
    F: Fn(&[u8]) -> Option<T> + 'static,
    N: Fn(&Value) -> Option<T> + 'static,
{
    Box::new(Fixed {
        ty: ty.clone(),
        out: Vec::with_capacity(rows),
        valid: Vec::with_capacity(rows),
        fast,
        native,
        wrap,
    })
}

/// A column of a fixed width type, parsed in place where `fast` can and cast where it cannot.
///
/// `native` takes the value the cast produced back out of its `Value`, which for a cast to the
/// column's own type is always the variant it names.
struct Fixed<T, F, N> {
    ty: LogicalType,
    out: Vec<T>,
    valid: Vec<bool>,
    fast: F,
    native: N,
    wrap: fn(Buffer<T>) -> Data,
}

impl<T, F, N> Build for Fixed<T, F, N>
where
    T: Copy + Default,
    F: Fn(&[u8]) -> Option<T>,
    N: Fn(&Value) -> Option<T>,
{
    fn rows(
        &mut self,
        cells: &Cells<'_>,
        column: usize,
        rows: Range<usize>,
        refuse: &dyn Fn(&str, usize) -> Error,
    ) -> Result<()> {
        for row in rows {
            let Some(span) = cells.at(row, column) else {
                self.out.push(T::default());
                self.valid.push(false);
                continue;
            };
            let parsed = if span.escaped() { None } else { (self.fast)(span.raw(cells.bytes)) };
            let value = match parsed {
                Some(value) => value,
                None => {
                    let cast = cast(cells, span, &self.ty, row, refuse)?;
                    (self.native)(&cast).ok_or_else(|| {
                        Error::internal(format!("{cast:?} does not belong in a {} column", self.ty))
                    })?
                }
            };
            self.out.push(value);
            self.valid.push(true);
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vector> {
        let Self { ty, out, valid, wrap, .. } = *self;
        Ok(Vector::flat(ty, wrap(Buffer::from_vec(out)))?.with_validity(Validity::from_run(&valid)))
    }
}

/// A `VARCHAR` column, copied once from the buffer into the column's arena.
///
/// A field that is valid UTF-8 and has no escape in it goes in as the bytes it is. The rest go in
/// as the text [`Span::text`] makes of them, which is the text the cell used to be.
///
/// The views and the arena are plain vectors until [`Build::finish`] makes them a column. Pushing
/// through the column's buffer asked whether it was a shared page on every row, and the compiler
/// kept that push out of line, which was about 2% of a `lineitem` load's samples.
struct Text {
    ty: LogicalType,
    views: Vec<StringView>,
    arena: Vec<u8>,
    valid: Vec<bool>,
}

impl Text {
    /// One string, laid the way [`StringColumn::push_bytes`] lays it.
    #[inline]
    fn push(&mut self, bytes: &[u8]) {
        let offset = self.arena.len() as u64;
        if bytes.len() > INLINE_LIMIT {
            self.arena.extend_from_slice(bytes);
        }
        self.views.push(StringView::over(bytes, offset));
    }
}

impl Build for Text {
    fn rows(
        &mut self,
        cells: &Cells<'_>,
        column: usize,
        rows: Range<usize>,
        _refuse: &dyn Fn(&str, usize) -> Error,
    ) -> Result<()> {
        for row in rows {
            let Some(span) = cells.at(row, column) else {
                self.views.push(StringView::empty());
                self.valid.push(false);
                continue;
            };
            let raw = span.raw(cells.bytes);
            if !span.escaped() && rudb_common::utf8::valid(raw) {
                self.push(raw);
            } else {
                self.push(span.text(cells.bytes, cells.dialect).as_bytes());
            }
            self.valid.push(true);
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vector> {
        let Self { ty, views, arena, valid } = *self;
        let strings = StringColumn::from_parts(views, arena.into());
        Ok(Vector::flat(ty, Data::Varlen(strings))
            .expect("a VARCHAR vector holds strings")
            .with_validity(Validity::from_run(&valid)))
    }
}

/// A column of a type with no parser here, one `Value` at a time the way every column used to be.
struct Values {
    ty: LogicalType,
    values: Vec<Value>,
}

impl Build for Values {
    fn rows(
        &mut self,
        cells: &Cells<'_>,
        column: usize,
        rows: Range<usize>,
        refuse: &dyn Fn(&str, usize) -> Error,
    ) -> Result<()> {
        for row in rows {
            self.values.push(match cells.at(row, column) {
                None => Value::Null,
                Some(span) => cast(cells, span, &self.ty, row, refuse)?,
            });
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vector> {
        Vector::from_values(self.ty, &self.values)
    }
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

/// A whole number written as an optional sign and up to twenty digits, if it fits in an `i64`.
///
/// The cast reads the same text as the same number, at any length, so a number too long or too big
/// for this is the cast's to read or refuse, and so is one too big for the column's type.
fn whole(raw: &[u8]) -> Option<i64> {
    let (negative, run) = signed(raw);
    let magnitude = digits(run)?;
    if negative { 0i64.checked_sub_unsigned(magnitude) } else { i64::try_from(magnitude).ok() }
}

/// A whole number that fits in a `u64`, written the way [`whole`] reads one.
///
/// A minus sign is taken in front of a zero and nothing else, because `-0` is a zero to the cast
/// and every other negative number is out of range for an unsigned column.
fn natural(raw: &[u8]) -> Option<u64> {
    let (negative, run) = signed(raw);
    let magnitude = digits(run)?;
    (!negative || magnitude == 0).then_some(magnitude)
}

/// The sign in front of a number, if there is one, and the rest of it.
fn signed(raw: &[u8]) -> (bool, &[u8]) {
    match raw {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, raw),
    }
}

/// A run of one to twenty ASCII digits as a number, or `None` when it is empty, longer, has a byte
/// in it that is not a digit, or is more than a `u64` holds.
///
/// The first `len % 8` digits are read one at a time and the rest eight at a time, see [`eight`],
/// each step after the first a multiplication by the same ten to the eighth. The short head is a
/// loop rather than a word padded with zeros because copying a slice of unknown length into the
/// pad is a call to `memcpy`, which cost more than the digits. Only a twenty digit number can
/// overflow, and the checked arithmetic is what refuses it.
fn digits(run: &[u8]) -> Option<u64> {
    if run.is_empty() || run.len() > 20 {
        return None;
    }
    let (head, words) = run.split_at(run.len() % 8);
    let mut value = 0;
    for &byte in head {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value * 10 + u64::from(digit);
    }
    for word in words.chunks_exact(8) {
        let word = eight(word.try_into().ok()?)?;
        value = value.checked_mul(100_000_000)?.checked_add(word)?;
    }
    Some(value)
}

/// Eight ASCII digits as the number they spell, or `None` when one of them is not a digit.
///
/// Loaded little endian, the first digit is the low byte. Taking `'0'` from every byte leaves each
/// one at most 9 if it was a digit. A byte below `'0'` has its top bit set by the subtraction and a
/// byte above `'9'` has it set by adding `0x46`, which carries into the top bit from `0x3a` up, so
/// one mask over the two finds a byte that is not a digit anywhere in the word. A borrow or a carry
/// crosses into the next byte only from a byte that fails on its own, so it cannot hide one.
///
/// Then three multiplications put the digits together, each one joining neighbours into a number
/// twice as wide: every byte times ten plus the byte after it gives the pairs, every pair times a
/// hundred plus the pair after it gives the fours, and every four times ten thousand plus the four
/// after it gives all eight. The mask after each step keeps every other sum, because the ones in
/// between join the end of one number to the start of the next.
fn eight(bytes: [u8; 8]) -> Option<u64> {
    let word = u64::from_le_bytes(bytes);
    let low = word.wrapping_sub(0x3030_3030_3030_3030);
    let high = word.wrapping_add(0x4646_4646_4646_4646);
    if (low | high) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let pairs = (low.wrapping_mul((10 << 8) + 1) >> 8) & 0x00ff_00ff_00ff_00ff;
    let fours = (pairs.wrapping_mul((100 << 16) + 1) >> 16) & 0x0000_ffff_0000_ffff;
    Some(fours.wrapping_mul((10_000 << 32) + 1) >> 32)
}

/// A double written with nothing but digits, a point, signs and an exponent.
///
/// Those are the bytes on which the cast's own reading comes down to `str::parse` with nothing
/// trimmed and no separators taken out, so this is that call and gives the cast's answer. The
/// common case of a short plain decimal is worked out directly first, see [`short_decimal`].
///
/// There is no float parser of our own behind this. `str::parse` is Eisel-Lemire in `core`, which
/// already reads most doubles with one multiplication of 128 bits and falls back to big numbers
/// only for the rare one that sits too close to halfway between two doubles to tell.
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

/// The powers of ten a short decimal's digits can be shifted by.
const TENS: [u64; 16] = {
    let mut tens = [1; 16];
    let mut at = 1;
    while at < 16 {
        tens[at] = tens[at - 1] * 10;
        at += 1;
    }
    tens
};

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
/// in general hard. The digits either side of the point are read by [`digits`], which is the same
/// eight at a time reading a whole number gets.
fn short_decimal(raw: &[u8]) -> Option<f64> {
    let (negative, rest) = signed(raw);
    let (whole, fraction) = match rest.iter().position(|&byte| byte == b'.') {
        Some(point) => (&rest[..point], &rest[point + 1..]),
        None => (rest, &[][..]),
    };
    let scale = fraction.len();
    if whole.len() + scale > 15 {
        return None;
    }
    let mantissa = match (whole.is_empty(), fraction.is_empty()) {
        (true, true) => return None,
        (false, true) => digits(whole)?,
        (true, false) => digits(fraction)?,
        (false, false) => digits(whole)? * TENS[scale] + digits(fraction)?,
    };
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
            "9223372036854775808",
            "-9223372036854775809",
            "+9223372036854775807",
            "0009223372036854775807",
            "0000000000000000001",
            "-0000000000000000001",
            "00000000000000000001",
            "000000000000000000001",
            "-00000000000000000000",
            "1234567890123456789",
            "-1234567890123456789",
            "9999999999999999999",
            "18446744073709551615",
            "18446744073709551616",
            "+18446744073709551615",
            "-18446744073709551615",
            "99999999999999999999",
            "12345678",
            "123456789",
            "1234567812345678",
            "12345678123456789",
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
                        natural(raw).and_then(|x| u8::try_from(x).ok()).map(Value::UTinyInt)
                    }
                    LogicalType::USmallInt => {
                        natural(raw).and_then(|x| u16::try_from(x).ok()).map(Value::USmallInt)
                    }
                    LogicalType::UInteger => {
                        natural(raw).and_then(|x| u32::try_from(x).ok()).map(Value::UInteger)
                    }
                    LogicalType::UBigInt => natural(raw).map(Value::UBigInt),
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

    /// The whole number parser as it was before it read eight digits at a time, one digit to a
    /// step and at most eighteen of them, kept as the reference the new one is held to.
    fn whole_by_the_digit(raw: &[u8]) -> Option<i64> {
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

    /// The short decimal parser as it was before it read eight digits at a time.
    fn short_decimal_by_the_digit(raw: &[u8]) -> Option<f64> {
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
        let number = mantissa as f64 / EXACT[scale];
        Some(if negative { -number } else { number })
    }

    /// What `str::parse` makes of the text, for a number of at most twenty digits after its sign.
    ///
    /// `parse` reads any number of leading zeros and the parsers here stop at twenty digits, so a
    /// longer run is one they leave to the cast and is `None` here too.
    fn parsed<T: std::str::FromStr>(raw: &[u8]) -> Option<T> {
        let run = match raw {
            [b'-' | b'+', rest @ ..] => rest,
            _ => raw,
        };
        if run.len() > 20 {
            return None;
        }
        std::str::from_utf8(raw).ok()?.parse().ok()
    }

    /// Holds every parser that reads digits to its reference on one text.
    ///
    /// `whole` is `str::parse::<i64>` and gives what the old parser gave wherever the old one gave
    /// anything. `natural` is `str::parse::<u64>` with a negative zero read as zero, which the old
    /// parser read it as. The short decimal is the old short decimal, to the bit.
    fn check(raw: &[u8]) {
        let signed = whole(raw);
        assert_eq!(signed, parsed::<i64>(raw), "whole {:?}", String::from_utf8_lossy(raw));
        if let Some(old) = whole_by_the_digit(raw) {
            assert_eq!(
                signed,
                Some(old),
                "whole against the old {:?}",
                String::from_utf8_lossy(raw)
            );
        }
        let unsigned = match raw {
            [b'-', ..] => parsed::<i64>(raw).filter(|&x| x == 0).map(|_| 0),
            _ => parsed::<u64>(raw),
        };
        assert_eq!(natural(raw), unsigned, "natural {:?}", String::from_utf8_lossy(raw));
        assert_eq!(
            short_decimal(raw).map(f64::to_bits),
            short_decimal_by_the_digit(raw).map(f64::to_bits),
            "short decimal {:?}",
            String::from_utf8_lossy(raw)
        );
    }

    /// A small xorshift generator, so that the random cases are the same on every run.
    struct Xorshift(u64);

    impl Xorshift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> usize {
            (self.next() % bound) as usize
        }

        fn digit(&mut self) -> u8 {
            b'0' + self.below(10) as u8
        }
    }

    /// Every byte value at every place in a word of digits, which is every way one byte can spoil
    /// a word, and the word's value whenever none does.
    #[test]
    fn eight_digits_at_once_are_the_digits_one_at_a_time() {
        let mut random = Xorshift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..64 {
            let mut bytes = [0u8; 8];
            bytes.iter_mut().for_each(|byte| *byte = random.digit());
            for at in 0..8 {
                for byte in 0..=u8::MAX {
                    let mut word = bytes;
                    word[at] = byte;
                    let expected = std::str::from_utf8(&word)
                        .ok()
                        .filter(|text| text.bytes().all(|b| b.is_ascii_digit()))
                        .map(|text| text.parse::<u64>().expect("eight digits"));
                    assert_eq!(eight(word), expected, "{word:?}");
                }
            }
        }
        assert_eq!(eight(*b"00000000"), Some(0));
        assert_eq!(eight(*b"99999999"), Some(99_999_999));
        assert_eq!(eight(*b"12345678"), Some(12_345_678));
    }

    /// Runs of every length from nothing to past the longest the parsers take, each with every
    /// sign, as all nines, all zeros and random digits, and with every byte value put in at every
    /// place.
    #[test]
    fn digits_of_every_length_read_as_parse_reads_them() {
        let mut random = Xorshift(0x2545_f491_4f6c_dd1d);
        for len in 0..=21 {
            let nines = vec![b'9'; len];
            let zeros = vec![b'0'; len];
            let mut runs = vec![nines, zeros];
            for _ in 0..32 {
                runs.push((0..len).map(|_| random.digit()).collect());
            }
            for run in &runs {
                for sign in [&b""[..], b"-", b"+", b"--", b"+-", b"-+", b"."] {
                    let text = [sign, run].concat();
                    check(&text);
                    for at in 0..text.len() {
                        for byte in 0..=u8::MAX {
                            let mut spoiled = text.clone();
                            spoiled[at] = byte;
                            check(&spoiled);
                        }
                    }
                }
            }
        }
    }

    /// The edges of every integer type, one either side of each, written plainly, with a plus and
    /// with leading zeros.
    #[test]
    fn the_edges_of_every_integer_type_read_as_parse_reads_them() {
        let edges: [(i128, i128); 8] = [
            (i8::MIN.into(), i8::MAX.into()),
            (i16::MIN.into(), i16::MAX.into()),
            (i32::MIN.into(), i32::MAX.into()),
            (i64::MIN.into(), i64::MAX.into()),
            (0, u8::MAX.into()),
            (0, u16::MAX.into()),
            (0, u32::MAX.into()),
            (0, u64::MAX.into()),
        ];
        for (low, high) in edges {
            for edge in [low - 1, low, low + 1, high - 1, high, high + 1] {
                let magnitude = edge.unsigned_abs();
                let sign = if edge < 0 { "-" } else { "" };
                for zeros in 0..4 {
                    let pad = "0".repeat(zeros);
                    check(format!("{sign}{pad}{magnitude}").as_bytes());
                    if edge >= 0 {
                        check(format!("+{pad}{magnitude}").as_bytes());
                        check(format!("-{pad}{magnitude}").as_bytes());
                    }
                }
            }
        }
    }

    /// Two million texts made mostly of digits with signs, points and other bytes mixed in at
    /// random, which reach the cases the tests above list and the ones nobody thought to list.
    #[test]
    fn random_texts_read_as_the_references_read_them() {
        const OTHER: &[u8] = b"+-.eE_x /:";
        let mut random = Xorshift(0xdead_beef_cafe_f00d);
        let mut text = Vec::with_capacity(24);
        for _ in 0..2_000_000 {
            text.clear();
            let len = random.below(23);
            let spoil = random.below(4);
            match random.below(4) {
                0 => text.push(b'-'),
                1 => text.push(b'+'),
                _ => {}
            }
            for _ in 0..len {
                let roll = random.below(64);
                text.push(match roll {
                    0 if spoil > 0 => OTHER[random.below(OTHER.len() as u64)],
                    1 if spoil > 1 => random.next() as u8,
                    2 | 3 => b'.',
                    _ => random.digit(),
                });
            }
            check(&text);
        }
    }

    /// Whole numbers of nineteen and twenty digits, which the old parser left to the cast and the
    /// new one reads, read as the cast reads them.
    #[test]
    fn long_whole_numbers_are_the_numbers_the_cast_reads() {
        let mut random = Xorshift(0x0123_4567_89ab_cdef);
        for _ in 0..20_000 {
            let len = 19 + random.below(2);
            let mut text: String = (0..len).map(|_| char::from(random.digit())).collect();
            if random.below(2) == 0 {
                text.insert(0, '-');
            }
            let raw = text.as_bytes();
            if let Some(fast) = whole(raw) {
                assert_eq!(Some(Value::BigInt(fast)), cast_text(&text, &LogicalType::BigInt));
            }
            if let Some(fast) = natural(raw) {
                assert_eq!(Some(Value::UBigInt(fast)), cast_text(&text, &LogicalType::UBigInt));
            }
        }
    }
}
