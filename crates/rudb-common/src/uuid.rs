//! The `UUID` type's text and bytes.
//!
//! A UUID is held the way the pin holds one, as the 128 bit number its sixteen bytes spell read
//! big end first, with the top bit flipped. The flip is what makes a signed comparison of the
//! number put two UUIDs in the order their text sorts in, so a sort, a `min` and a zone map over
//! the 128 bit lane need nothing of their own to get the pin's order.

use std::fmt;

/// The bit the stored form flips, which is the top one.
const FLIP: i128 = i128::MIN;

/// The stored form of a UUID's text, or `None` when the text is not one.
///
/// This is the pin's reading. The text may sit inside one pair of braces, a hyphen anywhere is
/// skipped, and what is left has to be exactly thirty two hex digits of either case. So the
/// canonical form, the bare thirty two digits and the braced forms all read, and so does a string
/// with hyphens in odd places, while a space anywhere does not.
#[must_use]
pub fn parse(text: &str) -> Option<i128> {
    let bytes = text.as_bytes();
    let braced = bytes.first() == Some(&b'{');
    if braced && (bytes.len() < 2 || bytes.last() != Some(&b'}')) {
        return None;
    }
    let inner = if braced { &bytes[1..bytes.len() - 1] } else { bytes };
    let mut number: u128 = 0;
    let mut digits = 0;
    for &byte in inner {
        if byte == b'-' {
            continue;
        }
        let digit = char::from(byte).to_digit(16)?;
        if digits == 32 {
            return None;
        }
        number = (number << 4) | u128::from(digit);
        digits += 1;
    }
    (digits == 32).then(|| from_number(number))
}

/// The stored form of the UUID whose bytes are `bytes`, in the order they are written.
#[must_use]
pub fn from_bytes(bytes: [u8; 16]) -> i128 {
    from_number(u128::from_be_bytes(bytes))
}

/// The sixteen bytes of a stored UUID, in the order they are written.
#[must_use]
pub fn to_bytes(held: i128) -> [u8; 16] {
    to_number(held).to_be_bytes()
}

/// The version a UUID says it is, the high half of its seventh byte, as the pin reads it.
///
/// The pin takes the version character of the printed UUID and subtracts `'0'` from it, so a
/// version of `a` to `f` comes back as 49 to 54 rather than 10 to 15. That is tamnd/duckdb#21, and
/// the answers here are the pin's.
#[must_use]
pub fn version(held: i128) -> u32 {
    let nibble = u32::try_from((to_number(held) >> 76) & 0xf).unwrap_or(0);
    if nibble < 10 { nibble } else { nibble + u32::from(b'a' - b'0') - 10 }
}

/// The first forty eight bits of a UUID, which a version 7 one spends on the milliseconds since
/// 1970 it was made at.
#[must_use]
pub fn millis(held: i128) -> i64 {
    i64::try_from(to_number(held) >> 80).unwrap_or(0)
}

/// Writes a stored UUID in the canonical form, lower case in five groups.
///
/// # Errors
///
/// When the formatter does.
pub fn write(f: &mut fmt::Formatter<'_>, held: i128) -> fmt::Result {
    let n = to_number(held);
    write!(
        f,
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        n >> 96,
        (n >> 80) & 0xffff,
        (n >> 64) & 0xffff,
        (n >> 48) & 0xffff,
        n & 0xffff_ffff_ffff
    )
}

/// The number a UUID's bytes spell, from its stored form, which is what a cast to `UHUGEINT` gives.
#[must_use]
pub fn to_number(held: i128) -> u128 {
    u128::from_ne_bytes((held ^ FLIP).to_ne_bytes())
}

/// The stored form of the number a UUID's bytes spell, which is what a cast from `UHUGEINT` gives.
#[must_use]
pub fn from_number(number: u128) -> i128 {
    i128::from_ne_bytes(number.to_ne_bytes()) ^ FLIP
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Shown(i128);

    impl fmt::Display for Shown {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write(f, self.0)
        }
    }

    fn text(input: &str) -> Option<String> {
        parse(input).map(|held| Shown(held).to_string())
    }

    #[test]
    fn every_form_the_pin_reads_is_read_and_printed_canonically() {
        let canonical = "5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f";
        for input in [
            "5ABF4945-15A5-4D27-8B25-1D9E0E9DDF0F",
            "{5ABF4945-15A5-4D27-8B25-1D9E0E9DDF0F}",
            "5ABF494515A54D278B251D9E0E9DDF0F",
            "{5abf494515a54d278b251d9e0e9ddf0f}",
            "5ABF-4945-15A5-4D27-8B25-1D9E-0E9D-DF0F",
            "-5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f-",
        ] {
            assert_eq!(text(input).as_deref(), Some(canonical), "{input}");
        }
        for input in [
            "",
            "{}",
            "zzz",
            "g",
            "5ABF4945-15A5-4D27-8B25-1D9E0E9DDF0",
            "5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f0",
            " 5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f",
            "{5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f",
            "5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f}",
            "{",
        ] {
            assert_eq!(parse(input), None, "{input}");
        }
    }

    #[test]
    fn the_stored_form_orders_the_way_the_text_does() {
        let held = |input: &str| parse(input).expect("a uuid");
        assert!(
            held("00000000-0000-0000-0000-000000000001")
                < held("7fffffff-0000-0000-0000-000000000000")
        );
        assert!(
            held("7fffffff-0000-0000-0000-000000000000")
                < held("80000000-0000-0000-0000-000000000000")
        );
        assert!(
            held("80000000-0000-0000-0000-000000000000")
                < held("ffffffff-0000-0000-0000-000000000000")
        );
        let bytes = to_bytes(held("5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f"));
        assert_eq!(bytes[0], 0x5a);
        assert_eq!(from_bytes(bytes), held("5abf4945-15a5-4d27-8b25-1d9e0e9ddf0f"));
    }

    #[test]
    fn the_version_and_the_milliseconds_are_read_the_pin_way() {
        let held = |nibble: &str| parse(&format!("5abf4945-15a5-{nibble}d27-8b25-1d9e0e9ddf0f"));
        let versions: Vec<u32> =
            ["0", "4", "9", "a", "f"].iter().map(|n| version(held(n).expect("a uuid"))).collect();
        assert_eq!(versions, [0, 4, 9, 49, 54]);
        let v7 = parse("0190a2b3-c4d5-7e6f-8a9b-0c1d2e3f4a5b").expect("a uuid");
        assert_eq!(millis(v7), 0x0190_a2b3_c4d5);
    }
}
