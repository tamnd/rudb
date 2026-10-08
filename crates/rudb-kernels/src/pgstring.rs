//! The string and `bytea` functions of `pg_proc`, by the name of the C function in `prosrc`, as
//! `varlena.c`, `oracle_compat.c`, `encode.c`, `quote.c`, `misc.c`, `mbutils.c` and `ascii.c` in
//! `src/backend/utils` write them.
//!
//! The functions are strict, except the ones of [`NULLS`], so the caller gives a null for a null
//! argument of the others. The database encoding is UTF-8, so a function that counts characters
//! counts the characters of UTF-8.

use std::borrow::Cow;

use rudb_common::{Error, LogicalType, Result, SqlState, Value, guc};
use rudb_pgtypes::keywords::quote_identifier;

use crate::hashing;

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "binary_decode",
    "binary_encode",
    "btrim",
    "btrim1",
    "byteaGetBit",
    "byteaGetByte",
    "byteaSetBit",
    "byteaSetByte",
    "bytea_bit_count",
    "bytea_reverse",
    "bytealtrim",
    "byteartrim",
    "byteatrim",
    "casefold",
    "chr",
    "crc32_bytea",
    "crc32c_bytea",
    "initcap",
    "lpad",
    "ltrim",
    "ltrim1",
    "parse_ident",
    "pg_convert",
    "pg_convert_from",
    "pg_convert_to",
    "quote_ident",
    "quote_literal",
    "quote_nullable",
    "repeat",
    "rpad",
    "rtrim",
    "rtrim1",
    "sha224_bytea",
    "sha256_bytea",
    "sha384_bytea",
    "sha512_bytea",
    "split_part",
    "text_format",
    "text_format_nv",
    "text_reverse",
    "text_to_array",
    "text_to_array_null",
    "to_ascii_default",
    "to_ascii_enc",
    "to_ascii_encname",
    "unistr",
];

/// The C functions of this module that are not strict: they see a null argument.
pub(crate) const NULLS: &[&str] =
    &["quote_nullable", "text_format", "text_format_nv", "text_to_array", "text_to_array_null"];

/// The functions of this module that return a set, each with the function that gives the same
/// values as an array. `string_to_table` is `string_to_array` with a row for each element.
pub(crate) const ROWS: &[(&str, &str)] =
    &[("text_to_table", "text_to_array"), ("text_to_table_null", "text_to_array_null")];

/// `MaxAllocSize`, the largest value PostgreSQL makes.
const MAX_ALLOC: usize = 0x3fff_ffff;

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    use Value::{BigInt, Blob, Boolean, Integer, Null, Varchar};
    let value = match (src, args) {
        ("binary_decode", [Varchar(data), Varchar(format)]) => Blob(decode(data, format)?),
        ("binary_encode", [Blob(data), Varchar(format)]) => Varchar(encode(data, format)?),
        ("btrim", [Varchar(text), Varchar(set)]) => Varchar(trim_text(text, set, true, true)),
        ("ltrim", [Varchar(text), Varchar(set)]) => Varchar(trim_text(text, set, true, false)),
        ("rtrim", [Varchar(text), Varchar(set)]) => Varchar(trim_text(text, set, false, true)),
        ("btrim1", [Varchar(text)]) => Varchar(text.trim_matches(' ').to_owned()),
        ("ltrim1", [Varchar(text)]) => Varchar(text.trim_start_matches(' ').to_owned()),
        ("rtrim1", [Varchar(text)]) => Varchar(text.trim_end_matches(' ').to_owned()),
        ("byteaGetBit", [Blob(bytes), BigInt(n)]) => {
            let (byte, bit) = bit_index(bytes, *n)?;
            Integer(i32::from(bytes[byte] >> bit & 1))
        }
        ("byteaGetByte", [Blob(bytes), Integer(n)]) => {
            Integer(i32::from(bytes[byte_index(bytes, *n)?]))
        }
        ("byteaSetBit", [Blob(bytes), BigInt(n), Integer(new)]) => {
            let (byte, bit) = bit_index(bytes, *n)?;
            if !matches!(new, 0 | 1) {
                return Err(invalid("new bit must be 0 or 1"));
            }
            let mut bytes = bytes.clone();
            bytes[byte] = bytes[byte] & !(1 << bit) | new.to_le_bytes()[0] << bit;
            Blob(bytes)
        }
        ("byteaSetByte", [Blob(bytes), Integer(n), Integer(new)]) => {
            let at = byte_index(bytes, *n)?;
            let mut bytes = bytes.clone();
            bytes[at] = new.to_le_bytes()[0];
            Blob(bytes)
        }
        ("bytea_bit_count", [Blob(bytes)]) => {
            BigInt(bytes.iter().map(|byte| i64::from(byte.count_ones())).sum())
        }
        ("bytea_reverse", [Blob(bytes)]) => Blob(bytes.iter().rev().copied().collect()),
        ("bytealtrim", [Blob(bytes), Blob(set)]) => Blob(trim_bytes(bytes, set, true, false)),
        ("byteartrim", [Blob(bytes), Blob(set)]) => Blob(trim_bytes(bytes, set, false, true)),
        ("byteatrim", [Blob(bytes), Blob(set)]) => Blob(trim_bytes(bytes, set, true, true)),
        ("casefold", [Varchar(text)]) => Varchar(text.chars().map(lower).collect()),
        ("chr", [Integer(code)]) => Varchar(chr(*code)?),
        ("crc32_bytea", [Blob(bytes)]) => BigInt(i64::from(hashing::crc32(bytes, CRC32))),
        ("crc32c_bytea", [Blob(bytes)]) => BigInt(i64::from(hashing::crc32(bytes, CRC32C))),
        ("initcap", [Varchar(text)]) => Varchar(initcap(text)),
        ("lpad", [Varchar(text), Integer(len), Varchar(fill)]) => {
            Varchar(pad(text, *len, fill, true)?)
        }
        ("rpad", [Varchar(text), Integer(len), Varchar(fill)]) => {
            Varchar(pad(text, *len, fill, false)?)
        }
        ("parse_ident", [Varchar(text), Boolean(strict)]) => {
            text_array(parse_ident(text, *strict)?.into_iter().map(Varchar).collect())
        }
        ("pg_convert", [Blob(bytes), Varchar(from), Varchar(to)]) => {
            let from = guc::encoding(from).ok_or_else(|| bad_encoding("source", from))?;
            let to = guc::encoding(to).ok_or_else(|| bad_encoding("destination", to))?;
            Blob(convert(bytes, from, to)?)
        }
        ("pg_convert_from", [Blob(bytes), Varchar(from)]) => {
            let from = guc::encoding(from).ok_or_else(|| bad_encoding("source", from))?;
            let converted = convert(bytes, from, UTF8)?;
            Varchar(String::from_utf8(converted).map_err(|_| Error::internal("convert_from"))?)
        }
        ("pg_convert_to", [Varchar(text), Varchar(to)]) => {
            let to = guc::encoding(to).ok_or_else(|| bad_encoding("destination", to))?;
            Blob(convert(text.as_bytes(), UTF8, to)?)
        }
        ("quote_ident", [Varchar(text)]) => Varchar(quote_identifier(text).into_owned()),
        ("quote_literal" | "quote_nullable", [Varchar(text)]) => Varchar(quote_literal(text)),
        ("quote_nullable", [Null]) => Varchar("NULL".to_owned()),
        ("repeat", [Varchar(text), Integer(count)]) => Varchar(repeat(text, *count)?),
        ("sha224_bytea", [Blob(bytes)]) => Blob(hashing::sha224(bytes).to_vec()),
        ("sha256_bytea", [Blob(bytes)]) => Blob(hashing::sha256(bytes).to_vec()),
        ("sha384_bytea", [Blob(bytes)]) => Blob(hashing::sha384(bytes).to_vec()),
        ("sha512_bytea", [Blob(bytes)]) => Blob(hashing::sha512(bytes).to_vec()),
        ("split_part", [Varchar(text), Varchar(separator), Integer(field)]) => {
            Varchar(split_part(text, separator, *field)?)
        }
        ("text_format" | "text_format_nv", [Null, ..]) => Null,
        ("text_format", [Varchar(format), Null]) => Varchar(text_format(format, &[])?),
        ("text_format", [Varchar(format), Value::List { values, .. }]) => {
            let args = values
                .iter()
                .map(|value| match value {
                    Null => Ok(None),
                    Varchar(text) => Ok(Some(text.as_str())),
                    value => Err(Error::internal(format!("format() over a {value:?}"))),
                })
                .collect::<Result<Vec<_>>>()?;
            Varchar(text_format(format, &args)?)
        }
        ("text_format_nv", [Varchar(format)]) => Varchar(text_format(format, &[])?),
        ("text_reverse", [Varchar(text)]) => Varchar(text.chars().rev().collect()),
        ("text_to_array", [text, separator]) => split_text(text, separator, &Null),
        ("text_to_array_null", [text, separator, null]) => split_text(text, separator, null),
        ("to_ascii_default", [Varchar(_)]) => return Err(no_ascii(UTF8)),
        ("to_ascii_enc", [Varchar(text), Integer(code)]) => {
            let encoding = usize::try_from(*code).ok().and_then(|code| ENCODINGS.get(code));
            let encoding = encoding.ok_or_else(|| {
                Error::invalid_input(format!("{code} is not a valid encoding code"))
                    .state(SqlState::UNDEFINED_OBJECT)
                    .unplaced()
            })?;
            Varchar(to_ascii(text, encoding)?)
        }
        ("to_ascii_encname", [Varchar(text), Varchar(name)]) => {
            let encoding = guc::encoding(name).ok_or_else(|| {
                Error::invalid_input(format!("{name} is not a valid encoding name"))
                    .state(SqlState::UNDEFINED_OBJECT)
                    .unplaced()
            })?;
            Varchar(to_ascii(text, encoding)?)
        }
        ("unistr", [Varchar(text)]) => Varchar(unistr(text)?),
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// The reflected polynomials of CRC-32 and of CRC-32C.
const CRC32: u32 = 0xedb8_8320;
const CRC32C: u32 = 0x82f6_3b78;

fn invalid(message: impl Into<String>) -> Error {
    Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE).unplaced()
}

fn too_large() -> Error {
    Error::invalid_input("requested length too large")
        .state(SqlState::PROGRAM_LIMIT_EXCEEDED)
        .unplaced()
}

fn unsupported(message: impl Into<String>) -> Error {
    Error::not_implemented(message).state(SqlState::FEATURE_NOT_SUPPORTED).unplaced()
}

/// A text array of one dimension.
fn text_array(values: Vec<Value>) -> Value {
    Value::List { element: LogicalType::Varchar, values }
}

/// The lower case of a character, by the simple mapping of one character to one character.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The upper case of a character, or the character when its upper case is more than one
/// character, as the simple mapping has it.
fn upper(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(upper), None) => upper,
        _ => c,
    }
}

/// `initcap`: each letter after a letter or a digit in lower case, and each other one in upper
/// case.
fn initcap(text: &str) -> String {
    let mut after_alnum = false;
    text.chars()
        .map(|c| {
            let mapped = if after_alnum { lower(c) } else { upper(c) };
            after_alnum = c.is_alphanumeric();
            mapped
        })
        .collect()
}

/// `chr`: the character with the code point `code`.
fn chr(code: i32) -> Result<String> {
    let limit = |message: String| {
        Error::invalid_input(message).state(SqlState::PROGRAM_LIMIT_EXCEEDED).unplaced()
    };
    let code = match u32::try_from(code) {
        Err(_) => return Err(invalid("character number must be positive")),
        Ok(0) => return Err(limit("null character not permitted".to_owned())),
        Ok(code) if code > 0x10_ffff => {
            return Err(limit(format!("requested character too large for encoding: {code}")));
        }
        Ok(code) => code,
    };
    char::from_u32(code)
        .map(String::from)
        .ok_or_else(|| limit(format!("requested character not valid for encoding: {code}")))
}

/// `lpad` and `rpad`: `text` cut to `len` characters, then filled to `len` characters with the
/// characters of `fill`, over and over, on the left or on the right.
fn pad(text: &str, len: i32, fill: &str, left: bool) -> Result<String> {
    let mut len = usize::try_from(len).unwrap_or(0);
    let (kept, cut) = match text.char_indices().nth(len) {
        Some((at, _)) => (len, &text[..at]),
        None => (text.chars().count(), text),
    };
    if fill.is_empty() {
        len = kept;
    }
    // The worst case of four bytes for each character, and the header of the value.
    if len > (MAX_ALLOC - 4) / 4 {
        return Err(too_large());
    }
    let filling: String = fill.chars().cycle().take(len - kept).collect();
    Ok(if left { filling + cut } else { cut.to_owned() + &filling })
}

/// `repeat`: `text`, `count` times.
fn repeat(text: &str, count: i32) -> Result<String> {
    let count = usize::try_from(count).unwrap_or(0);
    match count.checked_mul(text.len()).and_then(|len| len.checked_add(4)) {
        Some(len) if len <= MAX_ALLOC => Ok(text.repeat(count)),
        _ => Err(too_large()),
    }
}

/// `quote_literal_cstr`: the string in quotes, with each quote and each backslash doubled, and
/// an `E` before it when it has a backslash.
fn quote_literal(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 3);
    if text.contains('\\') {
        quoted.push('E');
    }
    quoted.push('\'');
    for c in text.chars() {
        if matches!(c, '\'' | '\\') {
            quoted.push(c);
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// `split_part`: the field `field` of `text` split at `separator`, from the right for a
/// negative field, or an empty string for a field that is not there.
fn split_part(text: &str, separator: &str, field: i32) -> Result<String> {
    if field == 0 {
        return Err(invalid("field position must not be zero"));
    }
    if text.is_empty() {
        return Ok(String::new());
    }
    if separator.is_empty() {
        return Ok(if matches!(field, 1 | -1) { text.to_owned() } else { String::new() });
    }
    let part = match usize::try_from(field) {
        Ok(field) => text.split(separator).nth(field - 1),
        Err(_) => {
            // The matches go from the left, as they do for a positive field.
            let parts: Vec<&str> = text.split(separator).collect();
            usize::try_from(field.unsigned_abs())
                .ok()
                .and_then(|back| parts.len().checked_sub(back))
                .map(|at| parts[at])
        }
    };
    Ok(part.unwrap_or_default().to_owned())
}

/// `string_to_array`: the fields of `text` split at `separator`, or the characters of `text`
/// for a null separator, with a null for each field that is `null`.
fn split_text(text: &Value, separator: &Value, null: &Value) -> Value {
    let Value::Varchar(text) = text else { return Value::Null };
    let null = null.as_str();
    let field = |part: &str| match null {
        Some(null) if null == part => Value::Null,
        _ => Value::Varchar(part.to_owned()),
    };
    let values = match separator {
        Value::Varchar(_) if text.is_empty() => Vec::new(),
        Value::Varchar(separator) if separator.is_empty() => vec![field(text)],
        Value::Varchar(separator) => text.split(separator.as_str()).map(field).collect(),
        _ => text.char_indices().map(|(at, c)| field(&text[at..at + c.len_utf8()])).collect(),
    };
    text_array(values)
}

/// `parse_ident`: the parts of a qualified name. A part in double quotes keeps its case, and a
/// part without them is in lower case. Unless `strict`, text after the last part ends the name.
fn parse_ident(text: &str, strict: bool) -> Result<Vec<String>> {
    let fail = |detail: Option<&str>| {
        let error = invalid(format!("string is not a valid identifier: \"{text}\""));
        match detail {
            Some(detail) => error.detail(detail),
            None => error,
        }
    };
    // `scanner_isspace`.
    let space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = |b: u8| b == b'_' || b.is_ascii_alphabetic() || b >= 0x80;
    let bytes = text.as_bytes();
    let byte = |at: usize| bytes.get(at).copied().unwrap_or(0);
    let mut at = 0;
    while space(byte(at)) {
        at += 1;
    }
    let mut parts = Vec::new();
    let mut after_dot = false;
    loop {
        if byte(at) == b'"' {
            let mut part = String::new();
            loop {
                let Some(end) = text[at + 1..].find('"').map(|end| at + 1 + end) else {
                    return Err(fail(Some("String has unclosed double quotes.")));
                };
                part.push_str(&text[at + 1..end]);
                if byte(end + 1) != b'"' {
                    at = end + 1;
                    break;
                }
                part.push('"');
                at = end + 1;
            }
            if part.is_empty() {
                return Err(fail(Some("Quoted identifier must not be empty.")));
            }
            parts.push(part);
        } else if start(byte(at)) {
            let from = at;
            at += 1;
            while start(byte(at)) || byte(at).is_ascii_digit() || byte(at) == b'$' {
                at += 1;
            }
            parts.push(text[from..at].to_ascii_lowercase());
        } else if byte(at) == b'.' {
            return Err(fail(Some("No valid identifier before \".\".")));
        } else if after_dot {
            return Err(fail(Some("No valid identifier after \".\".")));
        } else {
            return Err(fail(None));
        }
        while space(byte(at)) {
            at += 1;
        }
        match byte(at) {
            b'.' => {
                after_dot = true;
                at += 1;
                while space(byte(at)) {
                    at += 1;
                }
            }
            0 if at >= bytes.len() => break,
            _ if strict => return Err(fail(None)),
            _ => break,
        }
    }
    Ok(parts)
}

/// The hint of an error in the format string of `format()`.
const FORMAT_HINT: &str = "For a single \"%\" use \"%%\".";

/// One conversion of `format()`: `%[n$][-][*[n$] | width]type`.
struct Spec {
    /// The argument, or `None` for the next one.
    arg: Option<usize>,
    /// The argument of the width: `None` for none, `Some(0)` for the next one.
    width_arg: Option<usize>,
    left: bool,
    width: i32,
}

/// The format string of `format()` as it is read.
struct FormatCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl FormatCursor<'_> {
    fn peek(&self) -> u8 {
        self.bytes[self.at]
    }

    /// `ADVANCE_PARSE_POINTER`: a conversion cannot end at the end of the string.
    fn advance(&mut self) -> Result<()> {
        self.at += 1;
        match self.at < self.bytes.len() {
            true => Ok(()),
            false => Err(invalid("unterminated format() type specifier").hint(FORMAT_HINT)),
        }
    }

    /// `text_format_parse_digits`.
    fn digits(&mut self) -> Result<Option<i32>> {
        let mut value = None;
        while self.peek().is_ascii_digit() {
            let digit = i32::from(self.peek() - b'0');
            let next = value.unwrap_or(0i32).checked_mul(10).and_then(|v| v.checked_add(digit));
            value = Some(next.ok_or_else(|| {
                Error::out_of_range("number is out of range")
                    .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
                    .unplaced()
            })?);
            self.advance()?;
        }
        Ok(value)
    }

    /// `text_format_parse_format`: the conversion, with the cursor at its type.
    fn spec(&mut self) -> Result<Spec> {
        let zero = || invalid("format specifies argument 0, but arguments are numbered from 1");
        let mut spec = Spec { arg: None, width_arg: None, left: false, width: 0 };
        if let Some(n) = self.digits()? {
            if self.peek() != b'$' {
                spec.width = n;
                return Ok(spec);
            }
            spec.arg = Some(usize::try_from(n).ok().filter(|&n| n > 0).ok_or_else(zero)?);
            self.advance()?;
        }
        while self.peek() == b'-' {
            spec.left = true;
            self.advance()?;
        }
        if self.peek() == b'*' {
            self.advance()?;
            spec.width_arg = Some(match self.digits()? {
                Some(n) => {
                    if self.peek() != b'$' {
                        return Err(invalid("width argument position must be ended by \"$\""));
                    }
                    let n = usize::try_from(n).ok().filter(|&n| n > 0).ok_or_else(zero)?;
                    self.advance()?;
                    n
                }
                None => 0,
            });
        } else if let Some(n) = self.digits()? {
            spec.width = n;
        }
        Ok(spec)
    }
}

/// `text_format`: `format` with each conversion replaced by an argument. The arguments are the
/// text of each value by the output function of its type, or `None` for a null.
fn text_format(format: &str, args: &[Option<&str>]) -> Result<String> {
    let too_few = || invalid("too few arguments for format()");
    let mut cursor = FormatCursor { bytes: format.as_bytes(), at: 0 };
    let mut result = String::with_capacity(format.len());
    let mut copied = 0;
    let mut next = 1;
    while cursor.at < format.len() {
        if cursor.peek() != b'%' {
            cursor.at += 1;
            continue;
        }
        result.push_str(&format[copied..cursor.at]);
        cursor.advance()?;
        if cursor.peek() == b'%' {
            result.push('%');
        } else {
            let spec = cursor.spec()?;
            let conversion = cursor.peek();
            if !matches!(conversion, b's' | b'I' | b'L') {
                let c = format[cursor.at..].chars().next().unwrap_or_default();
                let message = format!("unrecognized format() type specifier \"{c}\"");
                return Err(invalid(message).hint(FORMAT_HINT));
            }
            let mut width = spec.width;
            if let Some(arg) = spec.width_arg {
                if arg > 0 {
                    next = arg;
                }
                let value = *args.get(next - 1).ok_or_else(too_few)?;
                next += 1;
                width = match value {
                    None => 0,
                    Some(text) => rudb_pgtypes::int4_in(text)
                        .map_err(|error| Error::from(error).unplaced())?,
                };
            }
            if let Some(arg) = spec.arg {
                next = arg;
            }
            let value = *args.get(next - 1).ok_or_else(too_few)?;
            next += 1;
            let text: Cow<'_, str> = match (value, conversion) {
                (None, b's') => Cow::Borrowed(""),
                (None, b'L') => Cow::Borrowed("NULL"),
                (None, _) => {
                    return Err(Error::invalid_input(
                        "null values cannot be formatted as an SQL identifier",
                    )
                    .state(SqlState::NULL_VALUE_NOT_ALLOWED)
                    .unplaced());
                }
                (Some(text), b'I') => quote_identifier(text),
                (Some(text), b'L') => Cow::Owned(quote_literal(text)),
                (Some(text), _) => Cow::Borrowed(text),
            };
            append_padded(&mut result, &text, spec.left, width)?;
        }
        cursor.at += 1;
        copied = cursor.at;
    }
    result.push_str(&format[copied..]);
    Ok(result)
}

/// `text_format_append_string`: `text` padded with spaces to `width` characters, on the right
/// for `left` or a negative width, else on the left.
fn append_padded(result: &mut String, text: &str, left: bool, width: i32) -> Result<()> {
    if width == 0 {
        result.push_str(text);
        return Ok(());
    }
    let left = left || width < 0;
    if width == i32::MIN {
        return Err(Error::out_of_range("number is out of range")
            .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
            .unplaced());
    }
    let width = usize::try_from(width.unsigned_abs()).unwrap_or(usize::MAX);
    let padding = width.saturating_sub(text.chars().count());
    if left {
        enlarge(result, text.len())?;
        result.push_str(text);
    }
    enlarge(result, padding)?;
    result.extend(std::iter::repeat_n(' ', padding));
    if !left {
        enlarge(result, text.len())?;
        result.push_str(text);
    }
    Ok(())
}

/// `enlargeStringInfo`: a string cannot grow to `MaxAllocSize`.
fn enlarge(result: &mut String, more: usize) -> Result<()> {
    if more < MAX_ALLOC.saturating_sub(result.len()) {
        result.reserve(more);
        return Ok(());
    }
    Err(Error::out_of_memory(format!(
        "string buffer exceeds maximum allowed length ({MAX_ALLOC} bytes)"
    ))
    .state(SqlState::PROGRAM_LIMIT_EXCEEDED)
    .detail(format!(
        "Cannot enlarge string buffer containing {} bytes by {more} more bytes.",
        result.len()
    ))
    .unplaced())
}

/// `bytea_get_bit` and the others: the byte and the bit of the bit `n`, the bit 0 being the
/// lowest bit of the first byte.
fn bit_index(bytes: &[u8], n: i64) -> Result<(usize, u32)> {
    let bits = i64::try_from(bytes.len()).map_or(i64::MAX, |len| len.saturating_mul(8));
    match usize::try_from(n) {
        Ok(at) if n < bits => Ok((at / 8, u32::try_from(at % 8).unwrap_or_default())),
        _ => Err(index_error(n, bits - 1)),
    }
}

fn byte_index(bytes: &[u8], n: i32) -> Result<usize> {
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    match usize::try_from(n) {
        Ok(at) if at < bytes.len() => Ok(at),
        _ => Err(index_error(i64::from(n), len - 1)),
    }
}

fn index_error(n: i64, last: i64) -> Error {
    Error::invalid_input(format!("index {n} out of valid range, 0..{last}"))
        .state(SqlState::ARRAY_SUBSCRIPT_ERROR)
        .unplaced()
}

/// `dotrim`: `text` without the characters of `set` at the start, at the end or at both.
fn trim_text(text: &str, set: &str, start: bool, end: bool) -> String {
    let mut text = text;
    if start {
        text = text.trim_start_matches(|c| set.contains(c));
    }
    if end {
        text = text.trim_end_matches(|c| set.contains(c));
    }
    text.to_owned()
}

/// `dobyteatrim`: `bytes` without the bytes of `set` at the start, at the end or at both.
fn trim_bytes(bytes: &[u8], set: &[u8], start: bool, end: bool) -> Vec<u8> {
    let mut from = 0;
    let mut to = bytes.len();
    if start {
        while from < to && set.contains(&bytes[from]) {
            from += 1;
        }
    }
    if end {
        while to > from && set.contains(&bytes[to - 1]) {
            to -= 1;
        }
    }
    bytes[from..to].to_vec()
}

/// The text forms of `encode` and `decode`.
#[derive(Clone, Copy)]
enum Codec {
    Base32Hex,
    Base64,
    Base64Url,
    Escape,
    Hex,
}

fn codec(name: &str) -> Result<Codec> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "base32hex" => Codec::Base32Hex,
        "base64" => Codec::Base64,
        "base64url" => Codec::Base64Url,
        "escape" => Codec::Escape,
        "hex" => Codec::Hex,
        _ => {
            return Err(invalid(format!("unrecognized encoding: \"{name}\"")).hint(
                "Valid encodings are \"base32hex\", \"base64\", \"base64url\", \"escape\", and \
                 \"hex\".",
            ));
        }
    })
}

/// `binary_encode`.
fn encode(bytes: &[u8], name: &str) -> Result<String> {
    Ok(match codec(name)? {
        Codec::Base32Hex => base32hex_encode(bytes),
        Codec::Base64 => base64_encode(bytes, false),
        Codec::Base64Url => base64_encode(bytes, true),
        Codec::Escape => escape_encode(bytes),
        Codec::Hex => hashing::lower_hex(bytes),
    })
}

/// `binary_decode`.
fn decode(text: &str, name: &str) -> Result<Vec<u8>> {
    match codec(name)? {
        Codec::Base32Hex => base32hex_decode(text),
        Codec::Base64 => base64_decode(text, false),
        Codec::Base64Url => base64_decode(text, true),
        Codec::Escape => escape_decode(text.as_bytes()),
        Codec::Hex => hex_decode(text),
    }
}

/// The whitespace that a decoder skips.
fn skipped(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn invalid_symbol(c: char, codec: &str) -> Error {
    invalid(format!("invalid symbol \"{c}\" found while decoding {codec} sequence"))
}

/// `hex_decode`: two hexadecimal digits for each byte, with whitespace between the bytes.
fn hex_decode(text: &str) -> Result<Vec<u8>> {
    let digit = |c: char| {
        c.to_digit(16)
            .and_then(|digit| u8::try_from(digit).ok())
            .ok_or_else(|| invalid(format!("invalid hexadecimal digit: \"{c}\"")))
    };
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if skipped(c) {
            continue;
        }
        let high = digit(c)?;
        let low = chars
            .next()
            .ok_or_else(|| invalid("invalid hexadecimal data: odd number of digits"))?;
        bytes.push(high << 4 | digit(low)?);
    }
    Ok(bytes)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `pg_base64_encode` and `pg_base64url_encode`. Base64 has a newline after each 76 characters
/// and pads the last group with `=`. Base64url has neither.
fn base64_encode(bytes: &[u8], url: bool) -> String {
    let alphabet = if url { BASE64URL } else { BASE64 };
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4 + bytes.len() / 57);
    let mut line = 0;
    for group in bytes.chunks(3) {
        let byte = |at: usize| u32::from(group.get(at).copied().unwrap_or(0));
        let buf = byte(0) << 16 | byte(1) << 8 | byte(2);
        let symbol = |shift: u32| char::from(alphabet[(buf >> shift & 0x3f) as usize]);
        text.push(symbol(18));
        text.push(symbol(12));
        match group.len() {
            3 => {
                text.push(symbol(6));
                text.push(symbol(0));
                line += 4;
                if !url && line >= 76 {
                    text.push('\n');
                    line = 0;
                }
            }
            2 => {
                text.push(symbol(6));
                if !url {
                    text.push('=');
                }
            }
            _ if !url => text.push_str("=="),
            _ => {}
        }
    }
    text
}

/// `pg_base64_decode` and `pg_base64url_decode`. Base64url can leave out the padding.
fn base64_decode(text: &str, url: bool) -> Result<Vec<u8>> {
    let name = if url { "base64url" } else { "base64" };
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3 + 2);
    let (mut buf, mut pos, mut end) = (0u32, 0, 0);
    for given in text.chars() {
        if skipped(given) {
            continue;
        }
        let c = match given {
            '-' if url => '+',
            '_' if url => '/',
            c => c,
        };
        let value = if c == '=' {
            if end == 0 {
                end = match pos {
                    2 => 1,
                    3 => 2,
                    _ => {
                        return Err(invalid(format!(
                            "unexpected \"=\" while decoding {name} sequence"
                        )));
                    }
                };
            }
            0
        } else {
            let at = BASE64.iter().position(|&symbol| char::from(symbol) == c);
            u32::try_from(at.ok_or_else(|| invalid_symbol(given, name))?).unwrap_or_default()
        };
        buf = buf << 6 | value;
        pos += 1;
        if pos == 4 {
            let [_, first, second, third] = buf.to_be_bytes();
            bytes.push(first);
            if end != 1 {
                bytes.push(second);
            }
            if end == 0 {
                bytes.push(third);
            }
            buf = 0;
            pos = 0;
        }
    }
    match pos {
        0 => {}
        2 if url => bytes.push((buf << 12).to_be_bytes()[1]),
        3 if url => bytes.extend_from_slice(&(buf << 6).to_be_bytes()[1..3]),
        _ => {
            return Err(invalid(format!("invalid {name} end sequence"))
                .hint("Input data is missing padding, is truncated, or is otherwise corrupted."));
        }
    }
    Ok(bytes)
}

const BASE32HEX: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

/// `base32hex_encode`: five bits for each character, padded with `=` to a multiple of eight
/// characters.
fn base32hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0);
    for &byte in bytes {
        buf = buf << 8 | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(char::from(BASE32HEX[(buf >> bits & 0x1f) as usize]));
            buf &= (1 << bits) - 1;
        }
    }
    if bits > 0 {
        text.push(char::from(BASE32HEX[(buf << (5 - bits) & 0x1f) as usize]));
    }
    while !text.len().is_multiple_of(8) {
        text.push('=');
    }
    text
}

/// `base32hex_decode`. The first `=` of a group must be where a group of one to four bytes
/// ends, and no symbol can come after it.
fn base32hex_decode(text: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buf, mut bits, mut pos, mut end) = (0u32, 0, 0, false);
    for c in text.chars() {
        if skipped(c) {
            continue;
        }
        if c == '=' {
            if !end {
                if !matches!(pos, 2 | 4 | 5 | 7) {
                    return Err(invalid("unexpected \"=\" while decoding base32hex sequence"));
                }
                end = true;
            }
            pos += 1;
            continue;
        }
        let value =
            c.to_digit(32).filter(|_| !end).ok_or_else(|| invalid_symbol(c, "base32hex"))?;
        buf = buf << 5 | value;
        bits += 5;
        pos += 1;
        while bits >= 8 {
            bits -= 8;
            bytes.push((buf >> bits).to_le_bytes()[0]);
            buf &= (1 << bits) - 1;
        }
        if pos == 8 {
            pos = 0;
        }
    }
    Ok(bytes)
}

/// `esc_encode`: a zero byte and each byte with the high bit as `\` and three octal digits, and
/// a backslash doubled.
fn escape_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            0 | 0x80.. => {
                text.push('\\');
                for digit in [byte >> 6, byte >> 3 & 7, byte & 7] {
                    text.push(char::from(b'0' + digit));
                }
            }
            b'\\' => text.push_str("\\\\"),
            byte => text.push(char::from(byte)),
        }
    }
    text
}

/// `esc_decode`: `\\` and `\` with three octal digits are escapes, and any other backslash is an
/// error.
fn escape_decode(text: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        if text[at] != b'\\' {
            bytes.push(text[at]);
            at += 1;
        } else if let [high @ b'0'..=b'3', middle @ b'0'..=b'7', low @ b'0'..=b'7', ..] =
            text[at + 1..]
        {
            bytes.push((high - b'0') << 6 | (middle - b'0') << 3 | (low - b'0'));
            at += 4;
        } else if text.get(at + 1) == Some(&b'\\') {
            bytes.push(b'\\');
            at += 2;
        } else {
            return Err(Error::invalid_input("invalid input syntax for type bytea")
                .state(SqlState::INVALID_TEXT_REPRESENTATION)
                .unplaced());
        }
    }
    Ok(bytes)
}

const UTF8: &str = "UTF8";

fn bad_encoding(which: &str, name: &str) -> Error {
    invalid(format!("invalid {which} encoding name \"{name}\""))
}

/// `pg_convert`: `bytes` in the encoding `from` checked, then in the encoding `to`. UTF-8,
/// LATIN1 and SQL_ASCII are the encodings that convert.
fn convert(bytes: &[u8], from: &str, to: &str) -> Result<Vec<u8>> {
    let known = |encoding: &str| matches!(encoding, UTF8 | "LATIN1" | "SQL_ASCII");
    if !known(from) || !known(to) {
        return Err(unsupported(format!("conversion between {from} and {to} is not supported")));
    }
    verify(bytes, from)?;
    if from == to || to == "SQL_ASCII" {
        return Ok(bytes.to_vec());
    }
    if from == "SQL_ASCII" {
        verify(bytes, to)?;
        return Ok(bytes.to_vec());
    }
    if from == "LATIN1" {
        return Ok(bytes.iter().map(|&byte| char::from(byte)).collect::<String>().into_bytes());
    }
    // From UTF-8, which `verify` checked, to LATIN1.
    let text = std::str::from_utf8(bytes).map_err(|_| Error::internal("convert"))?;
    text.chars()
        .map(|c| {
            u8::try_from(u32::from(c)).map_err(|_| {
                let mut buf = [0; 4];
                let shown: Vec<String> =
                    c.encode_utf8(&mut buf).bytes().map(|b| format!("0x{b:02x}")).collect();
                Error::invalid_input(format!(
                    "character with byte sequence {} in encoding \"UTF8\" has no equivalent in \
                     encoding \"{to}\"",
                    shown.join(" ")
                ))
                .state(SqlState::UNTRANSLATABLE_CHARACTER)
                .unplaced()
            })
        })
        .collect()
}

/// `pg_verify_mbstr`: UTF-8 must be valid, and no encoding has a zero byte.
fn verify(bytes: &[u8], encoding: &str) -> Result<()> {
    if encoding == UTF8 {
        return rudb_pgtypes::verify_utf8(bytes)
            .map(drop)
            .map_err(|error| Error::from(error).unplaced());
    }
    match bytes.contains(&0) {
        true => Err(Error::invalid_input(format!(
            "invalid byte sequence for encoding \"{encoding}\": 0x00"
        ))
        .state(SqlState::CHARACTER_NOT_IN_REPERTOIRE)
        .unplaced()),
        false => Ok(()),
    }
}

/// The encodings by their codes in `pg_wchar.h`.
const ENCODINGS: [&str; 42] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "MULE_INTERNAL",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

fn no_ascii(encoding: &str) -> Error {
    unsupported(format!("encoding conversion from {encoding} to ASCII not supported"))
}

/// The ASCII letters of the bytes from 160, or from 128 for WIN1250, as `ascii.c` has them.
const LATIN1: &[u8; 96] =
    b"  cL Y  \"Ca  -R     'u .,      ?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
const LATIN2: &[u8; 96] =
    b" A L LS \"SSTZ-ZZ a,l'ls ,sstz\"zzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt.";
const LATIN9: &[u8; 96] =
    b"  cL YS sCa  -R     Zu .z   EeY?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
const WIN1250: &[u8; 128] =
    b"  ' \"    %S<STZZ `'\"\".--  s>stzz   L A  \"CS  -RZ  ,l'u .,as L\"lzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt ";

/// `pg_to_ascii`: each byte of `text` as a character of `encoding`, in ASCII. A byte below 128
/// stays, a byte below the table is a space, and the table gives the others.
fn to_ascii(text: &str, encoding: &str) -> Result<String> {
    let (table, first): (&[u8], u8) = match encoding {
        "LATIN1" => (LATIN1, 160),
        "LATIN2" => (LATIN2, 160),
        "LATIN9" => (LATIN9, 160),
        "WIN1250" => (WIN1250, 128),
        _ => return Err(no_ascii(encoding)),
    };
    Ok(text
        .bytes()
        .map(|byte| match byte {
            0..0x80 => char::from(byte),
            byte if byte < first => ' ',
            byte => char::from(table[usize::from(byte - first)]),
        })
        .collect())
}

/// `unistr`: the text with the Unicode escapes `\XXXX`, `\+XXXXXX`, `\uXXXX` and `\UXXXXXXXX`,
/// and `\\` for a backslash. Two escapes of UTF-16 surrogates make one character.
fn unistr(text: &str) -> Result<String> {
    let pair = || {
        Error::invalid_input("invalid Unicode surrogate pair")
            .state(SqlState::SYNTAX_ERROR)
            .unplaced()
    };
    let bytes = text.as_bytes();
    let hex = |from: usize, count: usize| {
        let digits = bytes.get(from..from + count)?;
        digits.iter().all(u8::is_ascii_hexdigit).then(|| {
            digits.iter().fold(0u32, |value, &digit| {
                value << 4 | char::from(digit).to_digit(16).unwrap_or_default()
            })
        })
    };
    let mut result = String::with_capacity(text.len());
    let mut first: Option<u32> = None;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            if first.is_some() {
                return Err(pair());
            }
            let c = text[at..].chars().next().unwrap_or_default();
            result.push(c);
            at += c.len_utf8();
            continue;
        }
        if bytes.get(at + 1) == Some(&b'\\') {
            if first.is_some() {
                return Err(pair());
            }
            result.push('\\');
            at += 2;
            continue;
        }
        let (code, len) = match bytes.get(at + 1) {
            _ if hex(at + 1, 4).is_some() => (hex(at + 1, 4), 5),
            Some(b'u') => (hex(at + 2, 4), 6),
            Some(b'+') => (hex(at + 2, 6), 8),
            Some(b'U') => (hex(at + 2, 8), 10),
            _ => (None, 0),
        };
        let Some(mut code) = code else {
            return Err(Error::invalid_input("invalid Unicode escape")
                .state(SqlState::SYNTAX_ERROR)
                .hint("Unicode escapes must be \\XXXX, \\+XXXXXX, \\uXXXX, or \\UXXXXXXXX.")
                .unplaced());
        };
        if code == 0 || code > 0x10_ffff {
            return Err(invalid(format!("invalid Unicode code point: {code:04X}")));
        }
        let second = (0xdc00..0xe000).contains(&code);
        match first.take() {
            Some(high) if second => code = 0x10000 + ((high - 0xd800) << 10) + (code - 0xdc00),
            Some(_) => return Err(pair()),
            None if second => return Err(pair()),
            None => {}
        }
        if (0xd800..0xdc00).contains(&code) {
            first = Some(code);
        } else {
            result.push(char::from_u32(code).ok_or_else(pair)?);
        }
        at += len;
    }
    if first.is_some() {
        return Err(pair());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(src: &str, args: &[Value]) -> Result<String> {
        match call(src, args)? {
            Some(Value::Varchar(text)) => Ok(text),
            other => panic!("{src} gave {other:?}"),
        }
    }

    fn bytes(src: &str, args: &[Value]) -> Result<Vec<u8>> {
        match call(src, args)? {
            Some(Value::Blob(bytes)) => Ok(bytes),
            other => panic!("{src} gave {other:?}"),
        }
    }

    fn varchar(text: &str) -> Value {
        Value::Varchar(text.to_owned())
    }

    fn state<T: std::fmt::Debug>(result: Result<T>) -> String {
        let error = result.unwrap_err();
        format!("{} {}", error.reported_state(), error.message())
    }

    #[test]
    fn format_reads_each_conversion_as_text_format_does() {
        let format = |fmt: &str, args: &[Option<&str>]| text_format(fmt, args);
        assert_eq!(format("%s %s", &[Some("a"), Some("1")]).unwrap(), "a 1");
        assert_eq!(format("%2$s %1$s", &[Some("a"), Some("b")]).unwrap(), "b a");
        assert_eq!(format("%-5s|%5s|", &[Some("a"), Some("é")]).unwrap(), "a    |    é|");
        assert_eq!(
            format("%*s|%-*s|", &[Some("-3"), Some("a"), None, Some("b")]).unwrap(),
            "a  |b|"
        );
        assert_eq!(
            format("%I %L %L %s", &[Some("a b"), Some("it's"), None, None]).unwrap(),
            "\"a b\" 'it''s' NULL "
        );
        assert_eq!(format("%% x", &[]).unwrap(), "% x");
        for (fmt, args, expected) in [
            ("%s", &[][..], "22023 too few arguments for format()"),
            (
                "%0$s",
                &[Some("a")][..],
                "22023 format specifies argument 0, but arguments are numbered from 1",
            ),
            ("%z", &[Some("a")][..], "22023 unrecognized format() type specifier \"z\""),
            ("%é", &[Some("a")][..], "22023 unrecognized format() type specifier \"é\""),
            ("%1", &[Some("a")][..], "22023 unterminated format() type specifier"),
            ("%*1s", &[Some("a")][..], "22023 width argument position must be ended by \"$\""),
            ("%99999999999s", &[Some("a")][..], "22003 number is out of range"),
            (
                "%*s",
                &[Some("x"), Some("a")][..],
                "22P02 invalid input syntax for type integer: \"x\"",
            ),
            ("%I", &[None][..], "22004 null values cannot be formatted as an SQL identifier"),
        ] {
            assert_eq!(state(format(fmt, args)), expected, "{fmt}");
        }
    }

    #[test]
    fn the_codecs_of_encode_and_decode_are_the_codecs_of_postgresql() {
        let blob = |data: &[u8]| Value::Blob(data.to_vec());
        let encode = |data: &[u8], name: &str| text("binary_encode", &[blob(data), varchar(name)]);
        let decode =
            |data: &str, name: &str| bytes("binary_decode", &[varchar(data), varchar(name)]);
        assert_eq!(encode(b"abc", "HEX").unwrap(), "616263");
        assert_eq!(encode(b"\x01\x02", "base64").unwrap(), "AQI=");
        assert_eq!(encode(b"\x01\x02", "base64url").unwrap(), "AQI");
        assert_eq!(encode(&[0xab; 57], "base64").unwrap().matches('\n').count(), 1);
        assert_eq!(encode(b"\0\xff\\a", "escape").unwrap(), "\\000\\377\\\\a");
        assert_eq!(encode(b"f", "base32hex").unwrap(), "CO======");
        assert_eq!(encode(b"foobar", "base32hex").unwrap(), "CPNMUOJ1E8======");
        assert_eq!(decode("61 62\n", "hex").unwrap(), b"ab");
        assert_eq!(decode("YW Jj", "base64").unwrap(), b"abc");
        assert_eq!(decode("YW=j", "base64").unwrap(), b"a");
        assert_eq!(decode("YWI", "base64url").unwrap(), b"ab");
        assert_eq!(decode("\\\\\\101b", "escape").unwrap(), b"\\Ab");
        assert_eq!(decode("cpnmuoj1e8======", "base32hex").unwrap(), b"foobar");
        for (data, name, expected) in [
            ("6", "hex", "22023 invalid hexadecimal data: odd number of digits"),
            ("6g", "hex", "22023 invalid hexadecimal digit: \"g\""),
            ("Y", "base64", "22023 invalid base64 end sequence"),
            ("Y===", "base64", "22023 unexpected \"=\" while decoding base64 sequence"),
            (
                "Y!",
                "base64url",
                "22023 invalid symbol \"!\" found while decoding base64url sequence",
            ),
            ("C=", "base32hex", "22023 unexpected \"=\" while decoding base32hex sequence"),
            (
                "CO==C",
                "base32hex",
                "22023 invalid symbol \"C\" found while decoding base32hex sequence",
            ),
            ("a\\9", "escape", "22P02 invalid input syntax for type bytea"),
            ("x", "nope", "22023 unrecognized encoding: \"nope\""),
        ] {
            assert_eq!(state(decode(data, name)), expected, "{data} {name}");
        }
    }

    #[test]
    fn the_strings_functions_give_the_answers_of_postgresql() {
        let one = |src: &str, arg: &str| text(src, &[varchar(arg)]).unwrap();
        assert_eq!(one("initcap", "hello wORLD foo_bar 1abc ǆa"), "Hello World Foo_Bar 1abc Ǆa");
        assert_eq!(one("casefold", "ẞ ABC"), "ß abc");
        assert_eq!(one("text_reverse", "aé"), "éa");
        assert_eq!(one("unistr", "d\\0061t\\+000061 \\d83d\\de00 \\110000"), "data 😀 ᄀ00");
        assert_eq!(
            state(text("unistr", &[varchar("\\d800")])),
            "42601 invalid Unicode surrogate pair"
        );
        assert_eq!(state(text("unistr", &[varchar("\\u12")])), "42601 invalid Unicode escape");
        assert_eq!(one("quote_literal", "a\\b'"), "E'a\\\\b'''");
        let trim = |src: &str, arg: &str, set: &str| text(src, &[varchar(arg), varchar(set)]);
        assert_eq!(trim("btrim", "xyaxy", "yx").unwrap(), "a");
        assert_eq!(trim("ltrim", "ééaé", "é").unwrap(), "aé");
        assert_eq!(trim("rtrim", "ééaé", "é").unwrap(), "ééa");
        assert_eq!(trim("btrim", "a", "").unwrap(), "a");
        let pad = |text: &str, len: i32, fill: &str| {
            call("lpad", &[varchar(text), Value::Integer(len), varchar(fill)])
        };
        assert_eq!(pad("abc", 5, "xy").unwrap(), Some(varchar("xyabc")));
        assert_eq!(pad("abc", 2, "xy").unwrap(), Some(varchar("ab")));
        assert_eq!(pad("abc", 5, "").unwrap(), Some(varchar("abc")));
        assert_eq!(state(pad("abc", 300_000_000, " ")), "54000 requested length too large");
        let split = |field: i32| {
            text("split_part", &[varchar("a,b,c"), varchar(","), Value::Integer(field)])
        };
        assert_eq!([split(2).unwrap(), split(-1).unwrap(), split(-4).unwrap()], ["b", "c", ""]);
        assert_eq!(state(split(0)), "22023 field position must not be zero");
        let ident = |text: &str| call("parse_ident", &[varchar(text), Value::Boolean(true)]);
        let parts =
            |parts: &[&str]| Some(text_array(parts.iter().map(|part| varchar(part)).collect()));
        assert_eq!(ident(" a . \"B\"\"c\" ").unwrap(), parts(&["a", "B\"c"]));
        assert_eq!(state(ident("a.")), "22023 string is not a valid identifier: \"a.\"");
        assert_eq!(state(ident("\"\"")), "22023 string is not a valid identifier: \"\"\"\"");
        assert_eq!(
            state(text("chr", &[Value::Integer(55_296)])),
            "54000 requested character not valid for encoding: 55296"
        );
        let ascii = |name: Value| {
            text(
                if name.as_str().is_some() { "to_ascii_encname" } else { "to_ascii_enc" },
                &[varchar("aé"), name],
            )
        };
        assert_eq!(ascii(varchar("latin1")).unwrap(), "aAC");
        assert_eq!(ascii(Value::Integer(29)).unwrap(), "aAC");
        assert_eq!(ascii(varchar("LATIN2")).unwrap(), "aAS");
        assert_eq!(
            state(ascii(varchar("utf8"))),
            "0A000 encoding conversion from UTF8 to ASCII not supported"
        );
        assert_eq!(state(ascii(varchar("nope"))), "42704 nope is not a valid encoding name");
        assert_eq!(state(ascii(Value::Integer(42))), "42704 42 is not a valid encoding code");
    }

    #[test]
    fn convert_checks_the_source_and_converts_between_utf8_and_latin1() {
        assert_eq!(convert("é".as_bytes(), UTF8, "LATIN1").unwrap(), b"\xe9");
        assert_eq!(convert(b"\xe9", "LATIN1", UTF8).unwrap(), "é".as_bytes());
        assert_eq!(convert("é".as_bytes(), UTF8, "SQL_ASCII").unwrap(), "é".as_bytes());
        assert_eq!(
            state(convert("€".as_bytes(), UTF8, "LATIN1")),
            "22P05 character with byte sequence 0xe2 0x82 0xac in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\""
        );
        assert_eq!(
            state(convert(b"\xff", UTF8, UTF8)),
            "22021 invalid byte sequence for encoding \"UTF8\": 0xff"
        );
        assert_eq!(
            state(convert(b"a\0", "LATIN1", UTF8)),
            "22021 invalid byte sequence for encoding \"LATIN1\": 0x00"
        );
        assert_eq!(
            state(convert(b"a", "EUC_JP", UTF8)),
            "0A000 conversion between EUC_JP and UTF8 is not supported"
        );
    }
}
