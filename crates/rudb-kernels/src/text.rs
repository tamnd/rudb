//! The four string functions that have a grammar rule of their own: `substring`, `position`,
//! `trim` and `overlay`, plus the aliases upstream answers the same way.
//!
//! Every rule below was measured against `v2.0.0-dev84237` one statement at a time, because none of
//! it follows from the others. Indices are characters and not bytes, so `substring('héllo', 2, 2)`
//! is `él`. They are one based, and the window is the half open range that starts where the index
//! says: `substring('abcdef', 0, 3)` is `ab` and not `abc`, since the window is positions zero,
//! one and two and the string starts at one.
//!
//! A negative start counts back from the end the way a subscript does, so `substring('abcdef', -1)`
//! is `f`. A negative length is not an empty answer and not an error: it runs the window backwards
//! from the start, so `substring('abcdef', 4, -2)` is `bc`. Both of those clamp rather than raise,
//! and a start that is still off the string after counting back leaves nothing, which is why
//! `substring('abcdef', -10, 3)` is empty while `substring('abcdef', -10)` is the whole string.
//!
//! `trim` with no characters named strips the space and nothing else. A tab survives it upstream,
//! which was measured with `length(trim(chr(9) || 'a'))` coming back 2, so this is not a rule about
//! whitespace. With characters named it strips any of them, as a set of characters rather than as a
//! prefix, so `trim('xyaxy', 'xy')` is `a`.
//!
//! `overlay` is a prefix, the replacement and a suffix. The suffix starts at `start + length` and
//! never before the first character, and a negative length means the length of the replacement
//! rather than a walk backwards, which is the one place it parts company with `substring`:
//! `overlay('abcdef' PLACING 'XY' FROM 2 FOR -5)` is `aXYdef` and the two characters skipped are
//! the two in `XY`.
//!
//! Everything here is one value at a time, the way [`crate::subscript`] is, and for the same reason:
//! nothing on the board or in the TPC queries calls one of these over a column, and a call that does
//! counts itself in [`crate::fallback`] so the report says how often it happened.

use rudb_common::{Error, Result, Value};

/// `substring(text, start)` and `substring(text, start, length)` on one row.
pub(crate) fn substring(text: &Value, start: &Value, length: Option<&Value>) -> Result<Value> {
    let characters: Vec<char> = string(text)?.chars().collect();
    let start = whole(start)?;
    let length = match length {
        Some(held) => Some(whole(held)?),
        None => None,
    };
    let count = characters.len() as i128;
    let begin = if start < 0 { count + start + 1 } else { start };
    // A missing length runs to the end, and a negative one runs backwards from the start and stops
    // one before it, which is what makes the end exclusive on one side and inclusive on the other.
    let (from, to) = match length {
        None => (begin, count),
        Some(length) if length < 0 => (begin + length, begin - 1),
        Some(length) => (begin, begin + length - 1),
    };
    let (from, to) = (from.max(1), to.min(count));
    if from > to {
        return Ok(Value::Varchar(String::new()));
    }
    let kept: String = characters[(from - 1) as usize..to as usize].iter().collect();
    Ok(Value::Varchar(kept))
}

/// `position(haystack, needle)`, which `strpos` and `instr` are the other two spellings of.
///
/// One based, zero for a needle that is not there, and one for a needle that is empty. The answer
/// counts characters and not bytes, so `strpos('héllo', 'llo')` is 3 and not 4.
pub(crate) fn position(haystack: &Value, needle: &Value) -> Result<Value> {
    let (haystack, needle) = (string(haystack)?, string(needle)?);
    let found = match haystack.find(needle) {
        None => 0,
        Some(byte) => haystack[..byte].chars().count() as i64 + 1,
    };
    Ok(Value::BigInt(found))
}

/// `trim`, `ltrim` and `rtrim`, with the characters to strip or without them.
pub(crate) fn trim(name: &str, text: &Value, characters: Option<&Value>) -> Result<Value> {
    let text = string(text)?;
    let set: Vec<char> = match characters {
        Some(held) => string(held)?.chars().collect(),
        None => vec![' '],
    };
    let strip = |character: char| set.contains(&character);
    let kept = match name {
        "ltrim" => text.trim_start_matches(strip),
        "rtrim" => text.trim_end_matches(strip),
        _ => text.trim_matches(strip),
    };
    Ok(Value::Varchar(kept.to_string()))
}

/// `overlay(text, replacement, start)` and `overlay(text, replacement, start, length)` on one row.
pub(crate) fn overlay(
    text: &Value,
    replacement: &Value,
    start: &Value,
    length: Option<&Value>,
) -> Result<Value> {
    let characters: Vec<char> = string(text)?.chars().collect();
    let replacement = string(replacement)?;
    let start = whole(start)?;
    let length = match length {
        Some(held) => whole(held)?,
        None => replacement.chars().count() as i128,
    };
    // A negative length is the replacement's own length, so `FOR -1` and `FOR -5` cut the same
    // characters out and the answer is the one the three argument call would have given.
    let length = if length < 0 { replacement.chars().count() as i128 } else { length };
    let count = characters.len() as i128;
    let before = (start - 1).clamp(0, count) as usize;
    let after = (start + length).clamp(1, count + 1) as usize - 1;
    let mut out: String = characters[..before].iter().collect();
    out.push_str(replacement);
    out.extend(&characters[after..]);
    Ok(Value::Varchar(out))
}

/// The string an argument is, which the binder has already cast to a VARCHAR.
fn string(value: &Value) -> Result<&str> {
    match value {
        Value::Varchar(text) => Ok(text),
        other => Err(Error::internal(format!("a string function over a {}", other.logical_type()))),
    }
}

/// The number an index is, which the binder has already cast to a BIGINT.
fn whole(value: &Value) -> Result<i128> {
    value
        .as_i64()
        .map(i128::from)
        .ok_or_else(|| Error::internal(format!("a string index by a {}", value.logical_type())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Varchar(value.to_string())
    }

    fn at(index: i64) -> Value {
        Value::BigInt(index)
    }

    fn shown(value: Result<Value>) -> String {
        match value.expect("the call fails") {
            Value::Varchar(held) => held,
            other => panic!("{other} is not a string"),
        }
    }

    #[test]
    fn a_substring_window_starts_where_the_index_says_and_clamps_at_both_ends() {
        let abcdef = text("abcdef");
        let case = |start: i64, length: Option<i64>| {
            let length = length.map(at);
            shown(substring(&abcdef, &at(start), length.as_ref()))
        };
        assert_eq!(case(2, Some(3)), "bcd");
        assert_eq!(case(2, None), "bcdef");
        // The window is positions zero, one and two, and the string starts at one.
        assert_eq!(case(0, Some(3)), "ab");
        assert_eq!(case(0, None), "abcdef");
        assert_eq!(case(3, Some(0)), "");
        assert_eq!(case(10, Some(3)), "");
        assert_eq!(case(5, Some(100)), "ef");
    }

    #[test]
    fn a_negative_substring_index_counts_from_the_end_and_a_negative_length_runs_backwards() {
        let abcdef = text("abcdef");
        let case = |start: i64, length: Option<i64>| {
            let length = length.map(at);
            shown(substring(&abcdef, &at(start), length.as_ref()))
        };
        assert_eq!(case(-1, Some(3)), "f");
        assert_eq!(case(-2, None), "ef");
        // A start that is still off the string after counting back leaves nothing when there is a
        // length to run forwards from and the whole string when the window runs to the end.
        assert_eq!(case(-10, Some(3)), "");
        assert_eq!(case(-10, None), "abcdef");
        assert_eq!(case(3, Some(-1)), "b");
        assert_eq!(case(4, Some(-2)), "bc");
        assert_eq!(case(4, Some(-10)), "abc");
        assert_eq!(case(-2, Some(-1)), "d");
        assert_eq!(case(1, Some(-1)), "");
    }

    #[test]
    fn a_string_function_counts_characters_and_not_bytes() {
        assert_eq!(shown(substring(&text("héllo"), &at(2), Some(&at(2)))), "él");
        assert_eq!(shown(substring(&text("héllo"), &at(-2), Some(&at(2)))), "lo");
        assert_eq!(position(&text("héllo"), &text("llo")), Ok(Value::BigInt(3)));
        assert_eq!(shown(trim("trim", &text("héllo"), Some(&text("ho")))), "éll");
        assert_eq!(shown(overlay(&text("héllo"), &text("X"), &at(2), Some(&at(1)))), "hXllo");
    }

    #[test]
    fn a_needle_that_is_not_there_is_zero_and_one_that_is_empty_is_one() {
        assert_eq!(position(&text("abcdef"), &text("c")), Ok(Value::BigInt(3)));
        assert_eq!(position(&text("abcdef"), &text("z")), Ok(Value::BigInt(0)));
        assert_eq!(position(&text("abcdef"), &text("")), Ok(Value::BigInt(1)));
        assert_eq!(position(&text("abcdef"), &text("abc")), Ok(Value::BigInt(1)));
    }

    #[test]
    fn trimming_strips_a_set_of_characters_and_the_bare_form_strips_the_space_alone() {
        assert_eq!(shown(trim("trim", &text("  a  "), None)), "a");
        assert_eq!(shown(trim("ltrim", &text("  a  "), None)), "a  ");
        assert_eq!(shown(trim("rtrim", &text("  a  "), None)), "  a");
        // A tab is not a space and survives, which is upstream's rule and not an oversight here.
        assert_eq!(shown(trim("trim", &text("\ta"), None)), "\ta");
        assert_eq!(shown(trim("trim", &text("xyaxy"), Some(&text("xy")))), "a");
        assert_eq!(shown(trim("ltrim", &text("xxaxx"), Some(&text("x")))), "axx");
        assert_eq!(shown(trim("rtrim", &text("xxaxx"), Some(&text("x")))), "xxa");
        assert_eq!(shown(trim("trim", &text("xyaxy"), Some(&text("")))), "xyaxy");
        assert_eq!(shown(trim("trim", &text("aaa"), Some(&text("a")))), "");
    }

    #[test]
    fn an_overlay_cuts_as_many_characters_as_it_was_told_and_a_negative_count_is_the_replacement() {
        let abcdef = text("abcdef");
        let case = |replacement: &str, start: i64, length: Option<i64>| {
            let length = length.map(at);
            shown(overlay(&abcdef, &text(replacement), &at(start), length.as_ref()))
        };
        assert_eq!(case("X", 2, Some(1)), "aXcdef");
        assert_eq!(case("XY", 2, None), "aXYdef");
        assert_eq!(case("XY", 2, Some(0)), "aXYbcdef");
        assert_eq!(case("XY", 0, Some(2)), "XYbcdef");
        assert_eq!(case("XY", 10, Some(2)), "abcdefXY");
        assert_eq!(case("XY", 2, Some(100)), "aXY");
        assert_eq!(case("XY", 2, Some(-1)), "aXYdef");
        assert_eq!(case("XYZ", 2, Some(-1)), "aXYZef");
        assert_eq!(case("", 2, Some(-1)), "abcdef");
        // A start before the string keeps every character of it, since the suffix never begins
        // before the first one.
        assert_eq!(case("XY", -1, Some(2)), "XYabcdef");
        assert_eq!(case("XY", -5, Some(2)), "XYabcdef");
    }
}
