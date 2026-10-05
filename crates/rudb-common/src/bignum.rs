//! Integers of any size, the values of `BIGNUM`, laid out byte for byte the way the pin stores them.
//!
//! Three header bytes hold the number of data bytes with the top bit set, and the data bytes hold
//! the magnitude, most significant byte first, with no leading zero byte. Zero is one zero byte. A
//! negative number has every bit of its header and its data flipped. That makes the order of the
//! bytes the order of the numbers: a positive number starts with a set bit and a negative one with
//! a clear one, a longer positive number has a bigger header and a longer negative number a
//! smaller one, and two numbers of one length compare by their data. So a `BIGNUM` sorts, compares,
//! groups and hashes as its bytes do, and only arithmetic and the casts have to read one.
//!
//! The arithmetic works on the magnitude as 32 bit limbs, least significant first.

use crate::{Error, Result};

/// The bytes of the number with this sign and this magnitude, given most significant byte first.
#[must_use]
pub fn encode(negative: bool, magnitude: &[u8]) -> Vec<u8> {
    let start = magnitude.iter().position(|&byte| byte != 0).unwrap_or(magnitude.len());
    let data = if start == magnitude.len() { &[0][..] } else { &magnitude[start..] };
    let negative = negative && start < magnitude.len();
    #[expect(clippy::cast_possible_truncation, reason = "the length is kept in 23 bits")]
    let mut header = data.len() as u32 | 0x0080_0000;
    if negative {
        header = !header;
    }
    let mut bytes = Vec::with_capacity(data.len() + 3);
    bytes.extend_from_slice(&header.to_be_bytes()[1..]);
    if negative {
        bytes.extend(data.iter().map(|byte| !byte));
    } else {
        bytes.extend_from_slice(data);
    }
    bytes
}

/// Whether the number is below zero, and its magnitude, most significant byte first.
#[must_use]
pub fn decode(bytes: &[u8]) -> (bool, Vec<u8>) {
    let negative = bytes.first().is_some_and(|byte| byte & 0x80 == 0);
    let data = bytes.get(3..).unwrap_or_default();
    let magnitude = if negative { data.iter().map(|byte| !byte).collect() } else { data.to_vec() };
    (negative, magnitude)
}

/// The number `value`.
#[must_use]
pub fn from_i128(value: i128) -> Vec<u8> {
    encode(value < 0, &value.unsigned_abs().to_be_bytes())
}

/// The number `value`.
#[must_use]
pub fn from_u128(value: u128) -> Vec<u8> {
    encode(false, &value.to_be_bytes())
}

/// The sign and the magnitude, when the magnitude fits in 128 bits.
///
/// `Err` says which way it does not fit, true when the number is below zero, which is what picks
/// between the pin's two messages for a number too big for an integer type.
pub fn to_u128(bytes: &[u8]) -> std::result::Result<(bool, u128), bool> {
    let (negative, magnitude) = decode(bytes);
    if magnitude.len() > 16 {
        return Err(negative);
    }
    let mut wide = [0; 16];
    wide[16 - magnitude.len()..].copy_from_slice(&magnitude);
    Ok((negative, u128::from_be_bytes(wide)))
}

/// The number written in `text`, rounded to the nearest whole number with halves away from zero,
/// or `None` when `text` is not a number the pin reads.
///
/// The pin takes an optional sign, digits, and an optional point with digits after it, with at
/// least one digit somewhere. It rounds by the first digit after the point alone, so `0.49` is 0
/// and `0.5` is 1, and it takes no spaces, exponents or underscores.
#[must_use]
pub fn from_text(text: &str) -> Option<Vec<u8>> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) || !digits(fraction) || (whole.is_empty() && fraction.is_empty()) {
        return None;
    }
    let mut limbs = Vec::new();
    for chunk in whole.as_bytes().chunks(9) {
        let mut scale = 1;
        let mut value = 0;
        for byte in chunk {
            scale *= 10;
            value = value * 10 + u32::from(byte - b'0');
        }
        multiply_add(&mut limbs, scale, value);
    }
    if fraction.as_bytes().first().is_some_and(|&byte| byte >= b'5') {
        multiply_add(&mut limbs, 1, 1);
    }
    Some(encode(negative, &to_bytes(&limbs)))
}

/// The number as the pin writes it, in decimal with a minus sign when it is below zero.
#[must_use]
pub fn to_text(bytes: &[u8]) -> String {
    let (negative, magnitude) = decode(bytes);
    let mut limbs = to_limbs(&magnitude);
    let mut chunks = Vec::new();
    while !limbs.is_empty() {
        chunks.push(divide(&mut limbs, 1_000_000_000));
    }
    let mut text = String::with_capacity(chunks.len() * 9 + 1);
    if negative {
        text.push('-');
    }
    match chunks.split_last() {
        None => text.push('0'),
        Some((first, rest)) => {
            text.push_str(&first.to_string());
            for chunk in rest.iter().rev() {
                text.push_str(&format!("{chunk:09}"));
            }
        }
    }
    text
}

/// The whole part of `value`, or `None` when it is not a finite number.
#[must_use]
pub fn from_f64(value: f64) -> Option<Vec<u8>> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.trunc().abs().to_bits();
    let exponent = (bits >> 52) as i32;
    let fraction = bits & ((1 << 52) - 1);
    if exponent == 0 {
        // Below one, and the whole part of that is zero.
        return Some(encode(false, &[0]));
    }
    let mantissa = fraction | 1 << 52;
    let shift = exponent - 1075;
    let mut limbs = to_limbs(&mantissa.to_be_bytes());
    if shift < 0 {
        let mut whole = mantissa >> -shift;
        limbs.clear();
        while whole > 0 {
            #[expect(clippy::cast_possible_truncation, reason = "one limb at a time")]
            limbs.push(whole as u32);
            whole >>= 32;
        }
    } else {
        #[expect(clippy::cast_sign_loss, reason = "the shift is not negative here")]
        shift_left(&mut limbs, shift as u32);
    }
    Some(encode(value < 0.0, &to_bytes(&limbs)))
}

/// The nearest double, or `None` when the number is beyond the largest one.
#[must_use]
pub fn to_f64(bytes: &[u8]) -> Option<f64> {
    to_text(bytes).parse::<f64>().ok().filter(|value| value.is_finite())
}

/// `left + right`.
#[must_use]
pub fn add(left: &[u8], right: &[u8]) -> Vec<u8> {
    let (left_negative, left) = decode(left);
    let (right_negative, right) = decode(right);
    let (left, right) = (to_limbs(&left), to_limbs(&right));
    if left_negative == right_negative {
        return encode(left_negative, &to_bytes(&add_limbs(&left, &right)));
    }
    if compare_limbs(&left, &right) == std::cmp::Ordering::Less {
        encode(right_negative, &to_bytes(&subtract_limbs(&right, &left)))
    } else {
        encode(left_negative, &to_bytes(&subtract_limbs(&left, &right)))
    }
}

/// `left - right`.
#[must_use]
pub fn subtract(left: &[u8], right: &[u8]) -> Vec<u8> {
    add(left, &negate(right))
}

/// `-value`.
#[must_use]
pub fn negate(value: &[u8]) -> Vec<u8> {
    let (negative, magnitude) = decode(value);
    encode(!negative, &magnitude)
}

/// The pin's refusal of a string that is not a number, which names the wrong type the way the pin's
/// does.
#[must_use]
pub fn not_a_number(text: &str) -> Error {
    Error::conversion(format!("Could not convert string '{text}' to VARCHAR"))
}

/// The number in `text`, or the pin's refusal of it.
///
/// # Errors
///
/// When `text` is not a number, see [`from_text`].
pub fn parse(text: &str) -> Result<Vec<u8>> {
    from_text(text).ok_or_else(|| not_a_number(text))
}

/// Limbs, least significant first, from bytes, most significant first.
fn to_limbs(bytes: &[u8]) -> Vec<u32> {
    let mut limbs: Vec<u32> = bytes
        .rchunks(4)
        .map(|chunk| chunk.iter().fold(0, |limb, &byte| limb << 8 | u32::from(byte)))
        .collect();
    trim(&mut limbs);
    limbs
}

/// Bytes, most significant first and without leading zeros, from limbs, least significant first.
fn to_bytes(limbs: &[u32]) -> Vec<u8> {
    let bytes: Vec<u8> = limbs.iter().rev().flat_map(|limb| limb.to_be_bytes()).collect();
    let start = bytes.iter().position(|&byte| byte != 0).unwrap_or(bytes.len());
    if start == bytes.len() { vec![0] } else { bytes[start..].to_vec() }
}

fn trim(limbs: &mut Vec<u32>) {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
}

/// `limbs * scale + value`, in place.
fn multiply_add(limbs: &mut Vec<u32>, scale: u32, value: u32) {
    let mut carry = u64::from(value);
    for limb in limbs.iter_mut() {
        let product = u64::from(*limb) * u64::from(scale) + carry;
        #[expect(clippy::cast_possible_truncation, reason = "the low half")]
        {
            *limb = product as u32;
        }
        carry = product >> 32;
    }
    if carry > 0 {
        #[expect(clippy::cast_possible_truncation, reason = "the carry fits a limb")]
        limbs.push(carry as u32);
    }
}

/// Divides in place and returns the remainder.
fn divide(limbs: &mut Vec<u32>, divisor: u32) -> u32 {
    let mut remainder = 0_u64;
    for limb in limbs.iter_mut().rev() {
        let value = remainder << 32 | u64::from(*limb);
        #[expect(clippy::cast_possible_truncation, reason = "the quotient fits a limb")]
        {
            *limb = (value / u64::from(divisor)) as u32;
        }
        remainder = value % u64::from(divisor);
    }
    trim(limbs);
    #[expect(clippy::cast_possible_truncation, reason = "below the divisor")]
    {
        remainder as u32
    }
}

fn shift_left(limbs: &mut Vec<u32>, bits: u32) {
    let whole = (bits / 32) as usize;
    let part = bits % 32;
    if part > 0 {
        let mut carry = 0;
        for limb in limbs.iter_mut() {
            let next = *limb >> (32 - part);
            *limb = *limb << part | carry;
            carry = next;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    limbs.splice(0..0, std::iter::repeat_n(0, whole));
}

fn compare_limbs(left: &[u32], right: &[u32]) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.iter().rev().cmp(right.iter().rev()))
}

fn add_limbs(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut sum = Vec::with_capacity(left.len().max(right.len()) + 1);
    let mut carry = 0_u64;
    for at in 0..left.len().max(right.len()) {
        let total = u64::from(left.get(at).copied().unwrap_or(0))
            + u64::from(right.get(at).copied().unwrap_or(0))
            + carry;
        #[expect(clippy::cast_possible_truncation, reason = "the low half")]
        sum.push(total as u32);
        carry = total >> 32;
    }
    if carry > 0 {
        sum.push(1);
    }
    sum
}

/// `left - right`, where `left` is the larger.
fn subtract_limbs(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut difference = Vec::with_capacity(left.len());
    let mut borrow = 0_i64;
    for (at, &limb) in left.iter().enumerate() {
        let mut value = i64::from(limb) - i64::from(right.get(at).copied().unwrap_or(0)) - borrow;
        borrow = i64::from(value < 0);
        if value < 0 {
            value += 1 << 32;
        }
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "one limb")]
        difference.push(value as u32);
    }
    trim(&mut difference);
    difference
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02X}")).collect()
    }

    #[test]
    fn the_bytes_are_the_pins() {
        assert_eq!(hex(&from_i128(0)), "80000100");
        assert_eq!(hex(&from_i128(1)), "80000101");
        assert_eq!(hex(&from_i128(-1)), "7FFFFEFE");
        assert_eq!(hex(&from_i128(255)), "800001FF");
        assert_eq!(hex(&from_i128(256)), "8000020100");
        assert_eq!(hex(&from_i128(-256)), "7FFFFDFEFF");
    }

    #[test]
    fn the_bytes_sort_as_the_numbers_do() {
        let numbers = [-70_000_i128, -256, -255, -1, 0, 1, 255, 256, 70_000, i128::MAX];
        for pair in numbers.windows(2) {
            assert!(from_i128(pair[0]) < from_i128(pair[1]), "{pair:?}");
        }
    }

    #[test]
    fn text_goes_in_and_comes_back_out() {
        let big = "-340282366920938463463374607431768211455987";
        assert_eq!(to_text(&parse(big).expect("big")), big);
        for (text, number) in [("-0010.5", "-11"), ("0.49", "0"), (".5", "1"), ("+0", "0")] {
            assert_eq!(to_text(&parse(text).expect(text)), number, "{text}");
        }
        for text in ["", ".", "-", "+-0", "1e3", " 1", "1000.bla"] {
            assert!(from_text(text).is_none(), "{text}");
        }
    }

    #[test]
    fn arithmetic_carries_and_borrows_across_limbs() {
        let max = parse("9223372036854775808").expect("2^63");
        assert_eq!(to_text(&add(&max, &from_i128(1))), "9223372036854775809");
        assert_eq!(to_text(&add(&max, &from_i128(-1))), "9223372036854775807");
        assert_eq!(to_text(&subtract(&from_i128(10), &from_i128(7))), "3");
        assert_eq!(to_text(&subtract(&from_i128(7), &from_i128(10))), "-3");
        assert_eq!(to_text(&add(&from_i128(5), &from_i128(-5))), "0");
        assert_eq!(to_text(&negate(&from_i128(0))), "0");
    }

    #[test]
    fn a_double_keeps_its_whole_part() {
        assert_eq!(to_text(&from_f64(100_000.595).expect("small")), "100000");
        assert_eq!(to_text(&from_f64(-0.0).expect("zero")), "0");
        let text = to_text(&from_f64(f64::MAX).expect("max"));
        assert!(text.starts_with("17976931348623157081452742373170435679807056752584"));
        assert_eq!(to_f64(&from_f64(f64::MAX).expect("max")), Some(f64::MAX));
        assert!(from_f64(f64::NAN).is_none());
    }
}
