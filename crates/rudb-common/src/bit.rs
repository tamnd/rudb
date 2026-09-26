//! Bit strings, the values of `BIT`, laid out byte for byte the way the pin stores them.
//!
//! The first byte says how many bits of the second byte are padding, from 0 to 7, and the bits
//! themselves follow from the most significant bit of the second byte on. The padding bits are
//! always set, which is what keeps one bit string to one layout, so equal bit strings are equal
//! bytes and hash the same. A bit string has at least one bit. The order is not the order of the
//! bytes: the pin sorts bit strings by their bits read as text, so `01` sorts before `1` and `1`
//! before `111`, and [`cmp`] does the same.

use std::cmp::Ordering;

use crate::{Error, Result};

/// The bit string of `len` bits, all of them zero. `len` has to be at least one.
#[must_use]
pub fn zeros(len: usize) -> Vec<u8> {
    let padding = (8 - len % 8) % 8;
    let mut bits = vec![0; len.div_ceil(8) + 1];
    #[expect(clippy::cast_possible_truncation, reason = "the padding is below 8")]
    {
        bits[0] = padding as u8;
    }
    finalize(&mut bits);
    bits
}

/// How many bits a bit string has.
#[must_use]
pub fn len(bits: &[u8]) -> usize {
    (bits.len() - 1) * 8 - usize::from(bits[0])
}

/// Bit `n` of a bit string, counting from the left from 0.
#[must_use]
pub fn get(bits: &[u8], n: usize) -> bool {
    let at = n + usize::from(bits[0]);
    bits[at / 8 + 1] >> (7 - at % 8) & 1 == 1
}

/// Sets bit `n` of a bit string, counting from the left from 0.
pub fn set(bits: &mut [u8], n: usize, on: bool) {
    let at = n + usize::from(bits[0]);
    let mask = 1 << (7 - at % 8);
    if on {
        bits[at / 8 + 1] |= mask;
    } else {
        bits[at / 8 + 1] &= !mask;
    }
}

/// Sets the padding bits, which every bit string keeps set.
pub fn finalize(bits: &mut [u8]) {
    if bits.len() > 1 {
        bits[1] |= !(0xff_u8 >> bits[0]);
    }
}

/// How many of the bits are set.
#[must_use]
pub fn count(bits: &[u8]) -> usize {
    let set: u32 = bits[1..].iter().map(|byte| byte.count_ones()).sum();
    set as usize - usize::from(bits[0])
}

/// The bits as ones and zeros.
#[must_use]
pub fn to_text(bits: &[u8]) -> String {
    (0..len(bits)).map(|n| if get(bits, n) { '1' } else { '0' }).collect()
}

/// A bit string from ones and zeros, or from hex digits after an `x`, which count four bits each.
/// The empty string is the single bit `0`, as it is in the pin.
pub fn from_text(text: &str) -> Result<Vec<u8>> {
    if text.is_empty() {
        return Ok(zeros(1));
    }
    if let Some(hex) = text.strip_prefix('x') {
        if hex.is_empty() {
            return Err(Error::conversion("Cannot cast empty string to BIT"));
        }
        let mut bits = zeros(hex.len() * 4);
        for (at, c) in hex.chars().enumerate() {
            let digit = c.to_digit(16).ok_or_else(|| invalid(c))?;
            for bit in 0..4 {
                set(&mut bits, at * 4 + bit, digit >> (3 - bit) & 1 == 1);
            }
        }
        return Ok(bits);
    }
    let mut bits = zeros(text.chars().count());
    for (at, c) in text.chars().enumerate() {
        match c {
            '0' => {}
            '1' => set(&mut bits, at, true),
            other => return Err(invalid(other)),
        }
    }
    Ok(bits)
}

/// The error the pin raises for a character that is not a bit.
#[must_use]
pub fn invalid(c: char) -> Error {
    Error::conversion(format!("Invalid character encountered in string -> bit conversion: '{c}'"))
}

/// The bit string of some bytes, eight bits each, which is how the pin casts a blob and, with the
/// bytes big end first, a number.
#[must_use]
pub fn from_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(bytes.len() + 1);
    bits.push(0);
    bits.extend_from_slice(bytes);
    bits
}

/// The bytes a bit string casts to a blob as, with the padding bits of the first byte cleared.
#[must_use]
pub fn to_bytes(bits: &[u8]) -> Vec<u8> {
    let mut bytes = bits[1..].to_vec();
    bytes[0] &= 0xff_u8 >> bits[0];
    bytes
}

/// The two bit strings in the order the pin sorts them, bit by bit with a shorter one first when
/// it is the start of the other.
#[must_use]
pub fn cmp(left: &[u8], right: &[u8]) -> Ordering {
    let (left_len, right_len) = (len(left), len(right));
    for n in 0..left_len.min(right_len) {
        match get(left, n).cmp(&get(right, n)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    left_len.cmp(&right_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bit_string_keeps_the_pins_layout_and_prints_back_as_its_bits() {
        assert_eq!(from_text("0101").unwrap(), vec![4, 0xf5]);
        assert_eq!(from_text("00000000").unwrap(), vec![0, 0]);
        assert_eq!(from_text("000000000").unwrap(), vec![7, 0xfe, 0]);
        assert_eq!(from_text("x1f").unwrap(), from_text("00011111").unwrap());
        assert_eq!(from_text("").unwrap(), from_text("0").unwrap());
        for text in ["1", "0101", "111100001", "0000000011111111"] {
            let bits = from_text(text).unwrap();
            assert_eq!(to_text(&bits), text);
            assert_eq!(len(&bits), text.len());
            assert_eq!(count(&bits), text.matches('1').count());
        }
        assert!(from_text("012").is_err());
        assert!(from_text("x").is_err());
        assert_eq!(to_bytes(&from_text("0101").unwrap()), vec![5]);
    }

    #[test]
    fn bit_strings_sort_by_their_bits_as_text() {
        let mut texts = ["1", "01", "111", "00000000", "000000000"];
        texts.sort_by(|a, b| cmp(&from_text(a).unwrap(), &from_text(b).unwrap()));
        assert_eq!(texts, ["00000000", "000000000", "01", "1", "111"]);
    }
}
