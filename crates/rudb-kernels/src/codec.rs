//! The functions that write bytes or numbers as text and read them back: `hex`, `bin`, `unhex`,
//! `unbin`, `base64`, `from_base64`, `encode` and `decode`.
//!
//! Each one takes the steps the pin's takes, so a number is written without leading zeros, a
//! negative one as its two's complement, and an odd count of hexadecimal digits makes the first one
//! a byte of its own. The signature has already cast every argument to the type the pin declares.

use rudb_common::{Error, Result, Value};

/// The digits `hex` writes, which are upper case.
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// The characters base64 writes, in the order of the values they stand for.
const BASE64_DIGITS: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// What the pin says when a blob is not UTF-8 and nothing said what to do about it.
const NOT_UTF8: &str = "Failure in decode: could not convert blob to UTF8 string, the blob contained \
                        invalid UTF8 characters. \nUse try(decode(BLOB)) to return NULL and \
                        continue instead of returning an error. Specify decode(BLOB, 'replace') \
                        to replace invalid characters with '?'. Specify decode(BLOB, 'ignore') to \
                        remove invalid characters when encountered.";

/// `hex(value)` and `bin(value)`, which `binary` tells apart, over the one argument the signature
/// cast to a type the pin declares.
///
/// A string or a blob is written byte by byte. A number is written without leading zeros and a
/// negative one as its two's complement at its width, which is 64 bits for everything up to a
/// BIGINT, since that is the type the pin casts those to. A float is written as the BIGNUM the pin
/// casts it to.
pub(crate) fn written(binary: bool, value: &Value) -> Result<Value> {
    let number = |bits: u128| if binary { format!("{bits:b}") } else { format!("{bits:X}") };
    let bytes = |bytes: &[u8]| if binary { bin_of_bytes(bytes) } else { hex_of_bytes(bytes) };
    let float = |value: f64, type_name: &str| -> Result<String> {
        let shown = if value.is_nan() { "nan".to_string() } else { value.to_string() };
        Ok(bytes(&bignum_of_float(value, type_name, &shown)?))
    };
    Ok(Value::Varchar(match value {
        Value::Varchar(text) => bytes(text.as_bytes()),
        Value::Blob(blob) => bytes(blob),
        Value::BigInt(v) => number(u128::from(*v as u64)),
        Value::UBigInt(v) => number(u128::from(*v)),
        Value::HugeInt(v) => number(*v as u128),
        Value::UHugeInt(v) => number(*v),
        Value::Float(v) => float(f64::from(*v), "FLOAT")?,
        Value::Double(v) => float(*v, "DOUBLE")?,
        other => {
            return Err(Error::internal(format!(
                "hex or bin was handed a {}",
                other.logical_type()
            )));
        }
    }))
}

/// Every byte of `bytes` as two hexadecimal digits.
pub(crate) fn hex_of_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        out.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Every byte of `bytes` as eight binary digits.
pub(crate) fn bin_of_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:08b}")).collect()
}

/// A finite float as the pin's BIGNUM holds it, which is what `hex` and `bin` write for one.
///
/// The float is cut toward zero, so 2.5 is 2 and -0.7 is a negative zero. The bytes are a three
/// byte header with the top bit set and the count of bytes in the rest, then the magnitude big
/// endian with no leading zero bytes and at least one byte. A negative number is all of that with
/// every bit flipped, which is why `hex(-1.0)` is `7FFFFEFE`. The magnitude is the float's exact
/// value, so `hex(1e30)` is the digits of 1000000000000000019884624838656 and not of 10^30.
///
/// A float with no integer value is the pin's conversion error. Its words name VARCHAR as the type
/// it was cast to, which is the pin's own mistake and is kept, and the pin's cast from a FLOAT
/// raises it as an internal error, which is not.
pub(crate) fn bignum_of_float(value: f64, type_name: &str, shown: &str) -> Result<Vec<u8>> {
    if !value.is_finite() {
        return Err(Error::conversion(format!(
            "Type {type_name} with value {shown} can't be cast to the destination type VARCHAR"
        )));
    }
    let negative = value < 0.0;
    let whole = value.abs().trunc();
    // The magnitude little endian, built from the mantissa shifted by the exponent.
    let mut magnitude: Vec<u8> = Vec::new();
    if whole >= 1.0 {
        let bits = whole.to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as i64 - 1075;
        let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
        if exponent <= 0 {
            magnitude.extend_from_slice(&(mantissa >> -exponent).to_le_bytes());
        } else {
            let (bytes, bits) = ((exponent / 8) as usize, (exponent % 8) as u32);
            magnitude.resize(bytes, 0);
            let shifted = u128::from(mantissa) << bits;
            magnitude.extend_from_slice(&shifted.to_le_bytes());
        }
        while magnitude.last() == Some(&0) {
            magnitude.pop();
        }
    }
    if magnitude.is_empty() {
        magnitude.push(0);
    }
    magnitude.reverse();
    let header = 0x80_0000 | magnitude.len() as u32;
    let mut out = header.to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&magnitude);
    if negative {
        for byte in &mut out {
            *byte = !*byte;
        }
    }
    Ok(out)
}

/// `unhex(text)`. An odd count of digits makes the first digit a byte of its own.
pub(crate) fn unhex(text: &str) -> Result<Vec<u8>> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::invalid_input(format!(
            "Invalid input for hex digit: {}",
            String::from_utf8_lossy(&[byte])
        ))),
    };
    let bytes = text.as_bytes();
    let (first, rest) = bytes.split_at(bytes.len() % 2);
    let mut out = Vec::with_capacity(bytes.len().div_ceil(2));
    for &byte in first {
        out.push(digit(byte)?);
    }
    for pair in rest.chunks_exact(2) {
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Ok(out)
}

/// `unbin(text)`. A count of digits that is not a multiple of eight makes the leading ones a byte
/// of their own.
pub(crate) fn unbin(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    let (first, rest) = bytes.split_at(bytes.len() % 8);
    let mut out = Vec::with_capacity(bytes.len().div_ceil(8));
    let mut byte_of = |digits: &[u8]| -> Result<()> {
        let mut byte = 0u8;
        for &digit in digits {
            let bit = match digit {
                b'0' => 0,
                b'1' => 1,
                _ => {
                    return Err(Error::invalid_input(format!(
                        "Invalid input for binary digit: {}",
                        String::from_utf8_lossy(&[digit])
                    )));
                }
            };
            byte = (byte << 1) | bit;
        }
        out.push(byte);
        Ok(())
    };
    if !first.is_empty() {
        byte_of(first)?;
    }
    for digits in rest.chunks_exact(8) {
        byte_of(digits)?;
    }
    Ok(out)
}

/// `base64(blob)`, padded with `=` to a multiple of four characters.
pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let joined = group
            .iter()
            .enumerate()
            .fold(0u32, |held, (at, byte)| held | (u32::from(*byte) << (16 - 8 * at)));
        for at in 0..4 {
            if at <= group.len() {
                out.push(BASE64_DIGITS[((joined >> (18 - 6 * at)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `from_base64(text)`, step for step the pin's.
///
/// The length has to be a multiple of four. How many bytes come out is read off the last two
/// characters alone, and only the last group may hold `=` and only in its last two places, where it
/// stands for zero. That lets `YW=j` through as `a` with the `j` dropped, which is what the pin
/// answers, and is tamnd/duckdb#24.
pub(crate) fn from_base64(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(Error::conversion(format!(
            "Could not decode string \"{text}\" as base64: length must be a multiple of 4"
        )));
    }
    if bytes.len() < 4 {
        return Ok(Vec::new());
    }
    let mut size = bytes.len() / 4 * 3;
    if bytes[bytes.len() - 2] == b'=' {
        size -= 2;
    } else if bytes[bytes.len() - 1] == b'=' {
        size -= 1;
    }
    let value = |at: usize, padding: bool| -> Result<u32> {
        let byte = bytes[at];
        if padding && at % 4 >= 2 && byte == b'=' {
            return Ok(0);
        }
        match BASE64_DIGITS.iter().position(|digit| *digit == byte) {
            Some(value) => Ok(value as u32),
            None => Err(Error::conversion(format!(
                "Could not decode string \"{text}\" as base64: invalid byte value '{byte}' at \
                 position {at}"
            ))),
        }
    };
    let mut out = Vec::with_capacity(size);
    let last = bytes.len() - 4;
    for start in (0..bytes.len()).step_by(4) {
        let mut joined = 0u32;
        for at in start..start + 4 {
            joined = (joined << 6) | value(at, start == last)?;
        }
        for shift in [16, 8, 0] {
            if out.len() < size {
                out.push((joined >> shift) as u8);
            }
        }
    }
    Ok(out)
}

/// What `decode(blob, behavior)` does with bytes that are not UTF-8.
#[derive(Clone, Copy)]
enum Behavior {
    Strict,
    Replace,
    Ignore,
}

/// `decode(blob)`, and `decode(blob, behavior)` when `behavior` is given.
///
/// Bytes that are UTF-8 are the answer whatever the behavior says, so a behavior the pin does not
/// know is only refused when it would have been used.
pub(crate) fn decode(bytes: &[u8], behavior: Option<&str>) -> Result<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let behavior = match behavior {
        None => Behavior::Strict,
        Some(named) if named.eq_ignore_ascii_case("strict") => Behavior::Strict,
        Some(named) if named.eq_ignore_ascii_case("replace") => Behavior::Replace,
        Some(named) if named.eq_ignore_ascii_case("ignore") => Behavior::Ignore,
        Some(named) => {
            return Err(Error::conversion(format!(
                "decode error behavior specifier \"{named}\" not recognized"
            )));
        }
    };
    let kept = match behavior {
        Behavior::Strict => return Err(Error::conversion(NOT_UTF8)),
        Behavior::Replace => replaced(bytes),
        Behavior::Ignore => removed(bytes),
    };
    // Both walks leave nothing but whole sequences behind, so this cannot fail.
    String::from_utf8(kept).map_err(|_| Error::internal("a repaired blob is still not UTF-8"))
}

/// How one sequence that starts with a byte past ASCII turned out.
struct Sequence {
    /// Whether the sequence is a code point UTF-8 may hold.
    valid: bool,
    /// Whether it went wrong at a byte that does not continue a sequence, rather than at a code
    /// point it may not hold.
    mismatched: bool,
    /// Where it went wrong.
    wrong_at: usize,
    /// The last byte the walk looked at.
    last: usize,
}

/// The pin's walk over the sequence whose first byte is at `first`, which is `Utf8Proc`'s
/// `UTF8ExtraByteLoop`, or `None` when that byte cannot start one.
fn sequence(bytes: &[u8], first: usize) -> Option<Sequence> {
    let lead = bytes[first];
    let (extra, mask, mut code) = if lead & 0xe0 == 0xc0 {
        (1, 0x00_0780, u32::from(lead & 0x1f))
    } else if lead & 0xf0 == 0xe0 {
        (2, 0x00_f800, u32::from(lead & 0x0f))
    } else if lead & 0xf8 == 0xf0 {
        (3, 0x1f_0000, u32::from(lead & 0x07))
    } else {
        return None;
    };
    let wrong = |mismatched, wrong_at, last| Sequence { valid: false, mismatched, wrong_at, last };
    if bytes.len() - first < extra + 1 {
        return Some(wrong(true, first, first));
    }
    for (at, &byte) in bytes.iter().enumerate().skip(first + 1).take(extra) {
        if byte & 0xc0 != 0x80 {
            return Some(wrong(true, at, at));
        }
        code = (code << 6) | u32::from(byte & 0x3f);
    }
    let last = first + extra;
    if code & mask == 0 || code > 0x10_ffff || code & 0x1ff_f800 == 0xd800 {
        return Some(wrong(false, first, last));
    }
    Some(Sequence { valid: true, mismatched: false, wrong_at: first, last })
}

/// Every byte of a sequence that is not UTF-8 written as `?`, which is `Utf8Proc::MakeValid`.
///
/// A sequence that breaks off at a byte that does not continue it takes that byte with it, so the
/// `b` in `a\xC3b` is a `?` too.
fn replaced(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] & 0x80 != 0 {
            match sequence(bytes, at) {
                None => out[at] = b'?',
                Some(found) => {
                    if !found.valid {
                        out[at..=found.last].fill(b'?');
                    }
                    at = found.last;
                }
            }
        }
        at += 1;
    }
    out
}

/// Every sequence that is not UTF-8 left out, which is `Utf8Proc::RemoveInvalid`.
///
/// A sequence that breaks off at a byte that does not continue it leaves that byte to be read
/// again, so `a\xC3b` is `ab`.
fn removed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] & 0x80 == 0 {
            out.push(bytes[at]);
        } else if let Some(found) = sequence(bytes, at) {
            if found.valid {
                out.extend_from_slice(&bytes[at..=found.last]);
                at = found.last;
            } else if found.mismatched && found.last > at {
                at = found.wrong_at - 1;
            } else {
                at = found.last;
            }
        }
        at += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_float_is_held_the_way_the_pins_bignum_holds_it() {
        let hex = |value: f64| hex_of_bytes(&bignum_of_float(value, "DOUBLE", "").unwrap());
        assert_eq!(hex(1.5), "80000101");
        assert_eq!(hex(-2.5), "7FFFFEFD");
        assert_eq!(hex(-0.7), "7FFFFEFF");
        assert_eq!(hex(0.0), "80000100");
        assert_eq!(hex(-0.0), "80000100");
        assert_eq!(hex(65536.0), "800003010000");
        assert_eq!(hex(1e30), "80000D0C9F2C9CD04675000000000000");
    }

    #[test]
    fn a_blob_that_is_not_utf8_is_repaired_the_way_the_pin_repairs_it() {
        assert_eq!(replaced(b"a\xffb"), b"a?b");
        assert_eq!(replaced(b"a\xc3b"), b"a??");
        assert_eq!(replaced(b"\xf0\x9f\x98"), b"???");
        assert_eq!(replaced(b"\xe2\x82x\xe2\x82\xac"), "???\u{20ac}".as_bytes());
        assert_eq!(removed(b"a\xc3b"), b"ab");
        assert_eq!(removed(b"\xed\xa0\x80x"), b"x");
        assert_eq!(removed(b"\xe2\x82x\xe2\x82\xac"), "x\u{20ac}".as_bytes());
    }

    #[test]
    fn base64_goes_there_and_back() {
        for bytes in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"\x00\xff\x10"] {
            assert_eq!(from_base64(&base64(bytes)).unwrap(), bytes);
        }
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(from_base64("YW=j").unwrap(), b"a");
    }
}
