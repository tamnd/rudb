//! The functions that build a `json` or a `jsonb` value: `to_json`, `array_to_json`,
//! `row_to_json`, `json_build_array`, `json_build_object`, `json_object` and the `jsonb` form of
//! each.
//!
//! rudb keeps a `json` value as its text and a `jsonb` value as the text of its normal form, so a
//! function that builds a value writes text. Each value that goes into a new one comes here as its
//! JSON text already, which the binder writes by the rules of `datum_to_json_internal` in
//! `src/backend/utils/adt/json.c`. The functions here put the pieces together with the spaces that
//! PostgreSQL writes. A `jsonb` value is the `json` text through [`jsonb_in`], which gives it the
//! normal form, so the keys are in order and the last of two equal keys stays, as in `jsonb.c`.

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::json::{StackDepth, is_number};
use crate::jsonb::{escape_json, jsonb_in};

/// `IsValidJsonNumber`: whether the output text of a number is a number of JSON, which `to_json`
/// writes as it is. `NaN` and `Infinity` are not, and `to_json` writes them as strings.
#[must_use]
pub fn valid_json_number(text: &str) -> bool {
    is_number(text)
}

/// The JSON text of `piece` as a key. A key is a string, so a number or a boolean as a key is the
/// string of its text, as `datum_to_json_internal` writes it with `key_scalar`.
fn key(piece: &str, out: &mut String) {
    if piece.starts_with('"') {
        out.push_str(piece);
    } else {
        escape_json(piece, out);
    }
}

/// The `jsonb` value of the JSON text `text`.
fn binary(text: &str) -> Result<String, TypeError> {
    jsonb_in(text, StackDepth::default())
}

/// `array_to_json` and `row_to_json` with `pretty` set: a line feed and a space after each comma
/// between the elements or the fields of the outer array or object, and not in a value inside it.
#[must_use]
pub fn pretty_outer(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in text.chars() {
        out.push(c);
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 1 => out.push_str("\n "),
            _ => {}
        }
    }
    out
}

/// `json_build_array` and `jsonb_build_array` over the JSON text of each value, with `None` for
/// a null.
///
/// # Errors
///
/// Only for `jsonb`, if the value is nested deeper than the stack of PostgreSQL allows.
pub fn build_array(values: &[Option<&str>], jsonb: bool) -> Result<String, TypeError> {
    let mut out = String::from("[");
    for (at, value) in values.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(value.unwrap_or("null"));
    }
    out.push(']');
    if jsonb { binary(&out) } else { Ok(out) }
}

/// `json_build_object` and `jsonb_build_object` over the JSON text of each argument, keys and
/// values in turn, with `None` for a null.
///
/// # Errors
///
/// An odd number of arguments and a null key, with the errors of PostgreSQL, which differ between
/// the two functions.
pub fn build_object(args: &[Option<&str>], jsonb: bool) -> Result<String, TypeError> {
    let name = if jsonb { "jsonb_build_object" } else { "json_build_object" };
    if !args.len().is_multiple_of(2) {
        let mut error = TypeError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "argument list must have even number of elements".to_string(),
        );
        error.hint =
            Some(format!("The arguments of {name}() must consist of alternating keys and values."));
        return Err(error);
    }
    let mut out = String::from("{");
    for (at, pair) in args.chunks(2).enumerate() {
        let Some(name) = pair[0] else {
            return Err(match jsonb {
                true => TypeError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("argument {}: key must not be null", at * 2 + 1),
                ),
                false => null_key(),
            });
        };
        if at > 0 {
            out.push_str(", ");
        }
        key(name, &mut out);
        out.push_str(" : ");
        out.push_str(pair[1].unwrap_or("null"));
    }
    out.push('}');
    if jsonb { binary(&out) } else { Ok(out) }
}

/// `json_object` and `jsonb_object` over the keys and the values, each the text of an element of
/// a `text[]`, with `None` for a null.
///
/// # Errors
///
/// A null key.
pub fn object_of_texts(
    pairs: &[(Option<&str>, Option<&str>)],
    jsonb: bool,
) -> Result<String, TypeError> {
    let mut out = String::from("{");
    for (at, &(name, value)) in pairs.iter().enumerate() {
        let name = name.ok_or_else(null_key)?;
        if at > 0 {
            out.push_str(", ");
        }
        escape_json(name, &mut out);
        out.push_str(" : ");
        match value {
            Some(value) => escape_json(value, &mut out),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    if jsonb { binary(&out) } else { Ok(out) }
}

/// `to_jsonb`: the `jsonb` value of the JSON text of a value.
///
/// # Errors
///
/// If the value is nested deeper than the stack of PostgreSQL allows.
pub fn to_jsonb(text: &str) -> Result<String, TypeError> {
    binary(text)
}

/// The error of `json_object` and `json_build_object` for a null key.
fn null_key() -> TypeError {
    TypeError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key".into())
}

/// The error of `json_object` for an array of keys and values of the wrong shape.
#[must_use]
pub fn object_shape_error(message: &str) -> TypeError {
    TypeError::new(SqlState::ARRAY_SUBSCRIPT_ERROR, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_written_as_it_is_only_when_json_has_it() {
        for number in ["1", "-1.50", "1e400", "0.5", "-0"] {
            assert!(valid_json_number(number), "{number}");
        }
        for text in ["NaN", "Infinity", "-Infinity", "01", ".5", "1.", " 1", "1 "] {
            assert!(!valid_json_number(text), "{text}");
        }
    }

    #[test]
    fn pretty_breaks_the_line_only_between_the_outer_elements() {
        assert_eq!(pretty_outer("[[1,2],[3,4]]"), "[[1,2],\n [3,4]]");
        assert_eq!(
            pretty_outer(r#"{"f1":1,"f2":"a,\"b","f3":{"f1":3,"f2":4}}"#),
            "{\"f1\":1,\n \"f2\":\"a,\\\"b\",\n \"f3\":{\"f1\":3,\"f2\":4}}"
        );
        assert_eq!(pretty_outer("[]"), "[]");
    }

    #[test]
    fn the_builders_write_the_spaces_of_postgresql() {
        let array = build_array(&[Some("1"), Some("\"a\""), None, Some("{\"a\" : 1}")], false);
        assert_eq!(array.unwrap(), r#"[1, "a", null, {"a" : 1}]"#);
        let object = build_object(&[Some("\"a\""), Some("1"), Some("2"), Some("true")], false);
        assert_eq!(object.unwrap(), r#"{"a" : 1, "2" : true}"#);
        let object = build_object(&[Some("\"b\""), Some("1"), Some("true"), None], true);
        assert_eq!(object.unwrap(), r#"{"b": 1, "true": null}"#);
        assert_eq!(build_object(&[], true).unwrap(), "{}");
        assert_eq!(build_array(&[], false).unwrap(), "[]");
    }

    #[test]
    fn the_builders_refuse_what_postgresql_refuses() {
        let odd = build_object(&[Some("\"a\"")], true).unwrap_err();
        assert_eq!(odd.message, "argument list must have even number of elements");
        assert_eq!(
            odd.hint.as_deref(),
            Some(
                "The arguments of jsonb_build_object() must consist of alternating keys and values."
            )
        );
        let null = build_object(&[Some("1"), Some("2"), None, Some("1")], true).unwrap_err();
        assert_eq!(null.message, "argument 3: key must not be null");
        let null = build_object(&[None, Some("1")], false).unwrap_err();
        assert_eq!(null.message, "null value not allowed for object key");
        assert_eq!(null.sqlstate, SqlState::NULL_VALUE_NOT_ALLOWED);
    }

    #[test]
    fn an_object_of_texts_quotes_each_value() {
        let pairs = [(Some("a"), Some("1")), (Some("b"), None)];
        assert_eq!(object_of_texts(&pairs, false).unwrap(), r#"{"a" : "1", "b" : null}"#);
        let pairs = [(Some("b"), Some("1")), (Some("a"), Some("2"))];
        assert_eq!(object_of_texts(&pairs, true).unwrap(), r#"{"a": "2", "b": "1"}"#);
        assert!(object_of_texts(&[(None, Some("1"))], false).is_err());
    }
}
