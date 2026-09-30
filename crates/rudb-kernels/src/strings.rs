//! The functions that build a string out of pieces or rewrite one a character at a time:
//! `concat_ws`, `repeat`, `lpad`, `rpad`, `ascii`, `unicode`, `translate`, `url_encode`,
//! `url_decode`, `bar` and `to_base`.
//!
//! Each one takes the steps the pin's takes, so a character is a code point and not a grapheme, a
//! URL is encoded byte by byte, and a bar is cut into eighths of a block. The signature has already
//! cast every argument to the type the pin declares for it.

use rudb_common::{Error, LogicalType, Result, Value};

/// The largest string the pin can hold, in bytes.
const MOST_BYTES: u64 = u32::MAX as u64;

/// The most elements a repeated list may have here. The pin's own limit is on the bytes behind the
/// list rather than on the count and it is far past what fits in memory, so this one refuses in its
/// words before an allocation that could not succeed anyway.
const MOST_ELEMENTS: u64 = u32::MAX as u64;

/// A full block, and the blocks an eighth to seven eighths wide, which is what a bar is drawn with.
const FULL_BLOCK: &str = "\u{2588}";
const PARTIAL_BLOCKS: [&str; 8] =
    [" ", "\u{258f}", "\u{258e}", "\u{258d}", "\u{258c}", "\u{258b}", "\u{258a}", "\u{2589}"];

/// The digits `to_base` writes, which are upper case past nine.
const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// `concat_ws(separator, ...)` on one row, which is above the null rule.
///
/// A null separator is a null answer and a null piece is passed by, with no separator for it, so
/// `concat_ws(',', 'a', NULL, 'b')` is `a,b` and `concat_ws(',', NULL, NULL)` is the empty string.
/// A list is its elements, each a piece of its own, and a null element is passed by the same way.
pub(crate) fn concat_ws(arguments: &[Value]) -> Result<Value> {
    let Some((Value::Varchar(separator), rest)) = arguments.split_first() else {
        return Ok(Value::Null);
    };
    let mut out = String::new();
    let mut first = true;
    let mut push = |piece: &Value| -> Result<()> {
        match piece {
            Value::Null => {}
            Value::Varchar(text) => {
                if !first {
                    out.push_str(separator);
                }
                out.push_str(text);
                first = false;
            }
            other => {
                return Err(Error::internal(format!("concat_ws over a {}", other.logical_type())));
            }
        }
        Ok(())
    };
    for piece in rest {
        match piece {
            Value::List { values, .. } => values.iter().try_for_each(&mut push)?,
            other => push(other)?,
        }
    }
    Ok(Value::Varchar(out))
}

/// `repeat(held, count)` on one row, over a string, a blob or a list.
///
/// A count below one is the empty answer, and an answer too large for the pin to hold is refused in
/// its words.
pub(crate) fn repeat(held: &Value, count: &Value, returns: &LogicalType) -> Result<Value> {
    let count =
        count.as_i64().ok_or_else(|| Error::internal("repeat by a count that is not one"))?;
    let times = |length: usize| if count <= 0 || length == 0 { 0 } else { count.unsigned_abs() };
    match held {
        Value::Varchar(text) => {
            let times = times(text.len());
            let size = repeated_size(text.len(), times, "string")?;
            if size > MOST_BYTES {
                return Err(too_large_string(size));
            }
            Ok(Value::Varchar(text.repeat(times as usize)))
        }
        Value::Blob(bytes) => {
            let times = times(bytes.len());
            let size = repeated_size(bytes.len(), times, "string")?;
            if size > MOST_BYTES {
                return Err(too_large_string(size));
            }
            Ok(Value::Blob(bytes.repeat(times as usize)))
        }
        Value::List { values, element } => {
            let times = times(values.len());
            let size = repeated_size(values.len(), times, "list")?;
            if size > MOST_ELEMENTS {
                return Err(Error::out_of_range(format!(
                    "Cannot resize vector to {size} rows: maximum allowed vector size is 128.0 GiB"
                )));
            }
            let element = match returns {
                LogicalType::List(element) => (**element).clone(),
                _ => element.clone(),
            };
            let values =
                values.iter().cycle().take(values.len() * times as usize).cloned().collect();
            Ok(Value::List { element, values })
        }
        other => Err(Error::internal(format!("repeat over a {}", other.logical_type()))),
    }
}

/// How large `length` repeated `times` times is, or the pin's error when that does not fit in 64
/// bits.
fn repeated_size(length: usize, times: u64, what: &str) -> Result<u64> {
    (length as u64).checked_mul(times).ok_or_else(|| {
        if what == "list" {
            Error::out_of_range(format!(
                "Cannot create a list of size: '{length}' * '{times}', the result is too large"
            ))
        } else {
            Error::out_of_range(format!(
                "Cannot create a string of size: '{length}' * '{times}', the maximum supported \
                 string size is: '{MOST_BYTES}'"
            ))
        }
    })
}

/// The pin's error for a string longer than it can hold.
fn too_large_string(size: u64) -> Error {
    Error::out_of_range(format!(
        "Cannot create a string of size: '{size}', the maximum supported string size is: \
         '{MOST_BYTES}'"
    ))
}

/// `lpad(text, length, pad)` or `rpad(text, length, pad)` on one row.
///
/// The answer is `length` characters long. A text that is longer is cut to that many characters
/// from its start whichever side is padded, and a shorter one is padded with `pad` over and over,
/// cut wherever the count runs out. A negative length is zero, and an empty pad is refused only
/// when some padding is needed.
pub(crate) fn pad(name: &str, text: &str, length: i32, pad: &str) -> Result<Value> {
    let length = usize::try_from(length).unwrap_or(0);
    let (kept, counted) = match text.char_indices().nth(length) {
        Some((at, _)) => (&text[..at], length),
        None => (text, text.chars().count()),
    };
    let needed = length - counted;
    if needed > 0 && pad.is_empty() {
        let side = if name == "lpad" { "LPAD" } else { "RPAD" };
        return Err(Error::invalid_input(format!("Insufficient padding in {side}.")));
    }
    let padding: String = pad.chars().cycle().take(needed).collect();
    let mut out = String::with_capacity(kept.len() + padding.len());
    if name == "lpad" {
        out.push_str(&padding);
        out.push_str(kept);
    } else {
        out.push_str(kept);
        out.push_str(&padding);
    }
    Ok(Value::Varchar(out))
}

/// `ascii(text)` or `unicode(text)` on one row, the code point of the first character.
///
/// The two differ only on the empty string, which `ascii` answers with 0, the terminator the pin
/// reads past the end, and `unicode` answers with -1.
pub(crate) fn code_point(name: &str, text: &str) -> Value {
    match text.chars().next() {
        Some(first) => Value::Integer(first as i32),
        None if name == "ascii" => Value::Integer(0),
        None => Value::Integer(-1),
    }
}

/// `translate(text, from, to)` on one row.
///
/// Each character of `from` is replaced by the character at the same place in `to`, and one with no
/// place there is taken out. A character that is in `from` twice keeps its first meaning.
pub(crate) fn translate(text: &str, from: &str, to: &str) -> Value {
    let mut replaced: Vec<(char, Option<char>)> = Vec::new();
    let mut to = to.chars();
    for character in from.chars() {
        let meaning = to.next();
        if !replaced.iter().any(|(seen, _)| *seen == character) {
            replaced.push((character, meaning));
        }
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match replaced.iter().find(|(seen, _)| *seen == character) {
            Some((_, Some(meaning))) => out.push(*meaning),
            Some((_, None)) => {}
            None => out.push(character),
        }
    }
    Value::Varchar(out)
}

/// `url_encode(text)` on one row.
///
/// Letters, digits, `-`, `_`, `.` and `~` are kept and every other byte is written as `%` and two
/// upper case hex digits, the slash included.
pub(crate) fn url_encode(text: &str) -> Value {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'~' | b'.') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(DIGITS[usize::from(byte >> 4)]));
            out.push(char::from(DIGITS[usize::from(byte & 15)]));
        }
    }
    Value::Varchar(out)
}

/// `url_decode(text)` on one row.
///
/// A `%` followed by two hex digits is the byte they spell and anything else is kept as it is,
/// including a `+` and a `%` without two hex digits after it, so `abc%2` is kept whole. Bytes that
/// decode to something that is not UTF-8 are refused in the pin's words.
pub(crate) fn url_decode(text: &str) -> Result<Value> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'%'
            && at + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2]))
        {
            out.push((high << 4) | low);
            at += 3;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    String::from_utf8(out).map(Value::Varchar).map_err(|_| {
        Error::invalid_input(format!(
            "Failed to decode string \"{text}\" using URL decoding - decoded value is invalid UTF8"
        ))
    })
}

/// The value of one hex digit.
fn hex(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).and_then(|digit| u8::try_from(digit).ok())
}

/// `bar(x, min, max, width)` on one row, with a width of 80 when none is given.
///
/// The bar is `width * (x - min) / (max - min)` blocks long in eighths of a block, rounded down,
/// and padded with spaces to the whole width. At or below `min`, or when anything is not a number,
/// it is empty, and at or above `max` it is full.
pub(crate) fn bar(x: f64, min: f64, max: f64, most: f64) -> Result<Value> {
    if !most.is_finite() {
        return Err(Error::out_of_range("Max bar width must not be NaN or infinity"));
    }
    if most < 1.0 {
        return Err(Error::out_of_range("Max bar width must be >= 1"));
    }
    if most > 1000.0 {
        return Err(Error::out_of_range("Max bar width must be <= 1000"));
    }
    let width = if x.is_nan() || min.is_nan() || max.is_nan() || x <= min {
        0.0
    } else if x >= max {
        most
    } else {
        most * (x - min) / (max - min)
    };
    if !width.is_finite() {
        return Err(Error::out_of_range("Bar width must not be NaN or infinity"));
    }
    // The width is between 0 and 1000 here, so eight times it fits, and truncating is the pin's
    // cast.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let eighths = (width * 8.0) as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole_width = most as usize;
    let mut out = FULL_BLOCK.repeat(eighths / 8);
    let mut used = eighths / 8;
    if !eighths.is_multiple_of(8) {
        out.push_str(PARTIAL_BLOCKS[eighths % 8]);
        used += 1;
    }
    if used < whole_width {
        out.push_str(&" ".repeat(whole_width - used));
    }
    Ok(Value::Varchar(out))
}

/// `to_base(number, radix, min_length)` on one row, with a `min_length` of 0 when none is given.
pub(crate) fn to_base(number: i64, radix: i32, min_length: i32) -> Result<Value> {
    if number < 0 {
        return Err(Error::invalid_input("'to_base' number must be greater than or equal to 0"));
    }
    if !(2..=36).contains(&radix) {
        return Err(Error::invalid_input("'to_base' radix must be between 2 and 36"));
    }
    if !(0..=64).contains(&min_length) {
        return Err(Error::invalid_input("'to_base' min_length must be between 0 and 64"));
    }
    let radix = radix.unsigned_abs() as u64;
    let mut number = number.unsigned_abs();
    let mut digits = Vec::with_capacity(64);
    loop {
        digits.push(DIGITS[(number % radix) as usize]);
        number /= radix;
        if number == 0 {
            break;
        }
    }
    let min_length = min_length.unsigned_abs() as usize;
    while digits.len() < min_length {
        digits.push(b'0');
    }
    digits.reverse();
    Ok(Value::Varchar(digits.into_iter().map(char::from).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: Value) -> String {
        match value {
            Value::Varchar(text) => text,
            other => panic!("not a string: {other:?}"),
        }
    }

    #[test]
    fn padding_counts_characters_and_cycles_the_pad() {
        assert_eq!(text(pad("lpad", "abc", 8, "xyz").unwrap()), "xyzxyabc");
        assert_eq!(text(pad("rpad", "abc", 8, "xyz").unwrap()), "abcxyzxy");
        assert_eq!(text(pad("lpad", "héllo", 7, "ö").unwrap()), "ööhéllo");
        assert_eq!(text(pad("rpad", "abc", 2, "x").unwrap()), "ab");
        assert_eq!(text(pad("lpad", "abc", -1, "x").unwrap()), "");
        assert_eq!(text(pad("lpad", "abc", 2, "").unwrap()), "ab");
        assert!(pad("lpad", "abc", 5, "").is_err());
    }

    #[test]
    fn a_bar_is_cut_into_eighths() {
        assert_eq!(text(bar(3.3, 0.0, 10.0, 7.0).unwrap()), "\u{2588}\u{2588}\u{258e}    ");
        assert_eq!(text(bar(5.0, 0.0, 10.0, 2.5).unwrap()), "\u{2588}\u{258e}");
        assert_eq!(text(bar(-1.0, 0.0, 10.0, 3.0).unwrap()), "   ");
        assert!(bar(1e308, -1e308, f64::INFINITY, 3.0).is_err());
    }

    #[test]
    fn a_url_is_encoded_byte_by_byte() {
        assert_eq!(text(url_encode("a b/c?é~")), "a%20b%2Fc%3F%C3%A9~");
        assert_eq!(text(url_decode("a%20b+%e2%82%ac").unwrap()), "a b+€");
        assert_eq!(text(url_decode("abc%2").unwrap()), "abc%2");
        assert_eq!(text(url_decode("%%41").unwrap()), "%A");
        assert!(url_decode("%FF").is_err());
    }

    #[test]
    fn a_number_is_written_in_any_base_up_to_36() {
        assert_eq!(text(to_base(255, 2, 12).unwrap()), "000011111111");
        assert_eq!(text(to_base(i64::MAX, 36, 0).unwrap()), "1Y2P0IJ32E8E7");
        assert_eq!(text(to_base(0, 10, 0).unwrap()), "0");
        assert!(to_base(-1, 10, 0).is_err());
    }
}
