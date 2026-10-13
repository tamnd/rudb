//! The order of two `jsonb` values and the casts of a `jsonb` scalar to `boolean`, `numeric`,
//! `smallint`, `integer`, `bigint`, `real` and `double precision`.
//!
//! The order is the one of `compareJsonbContainers` in `src/backend/utils/adt/jsonb_util.c`. The
//! two values are walked as `JsonbIteratorNext` walks them, and the first step that differs
//! decides. An object is after an array, an array after a boolean, a boolean after a number, a
//! number after a string and a string after `null`. Of two arrays or two objects, the one with more
//! elements or more fields is after the other. Two strings are in the order of their bytes, which
//! is the order of the collation `C`. Two numbers are in the order of their values, so `1.0` and
//! `1` are equal.
//!
//! The casts are `jsonb_bool`, `jsonb_numeric` and the others of `src/backend/utils/adt/jsonb.c`.
//! Each takes a scalar of the one kind that fits the type, and a `null` gives a null.
//!
//! The walk takes no recursion, so a value nested to any depth is safe.

use std::cmp::Ordering;

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::float::{float4_in, float8_in};
use crate::json::Kind;
use crate::jsonfuncs::{Doc, Shape};
use crate::numeric::{Numeric, numeric_in, numeric_out};

/// A step of the walk of a value, as `JsonbIteratorNext` gives it with `skipNested` unset.
#[derive(Clone, Copy)]
enum Step<'d> {
    /// The start of an array, with the number of its elements and whether it holds a scalar at the
    /// top of a value, the "raw scalar" of PostgreSQL.
    Array(usize, bool),
    /// The start of an object, with the number of its fields.
    Object(usize),
    Key(&'d str),
    /// A scalar, an element of an array or the value of a field.
    Scalar(usize),
    End,
}

/// What is left to walk.
enum Visit<'d> {
    Node(usize),
    Key(&'d str),
    End,
}

/// The steps of the walk of `doc`.
fn walk<'d>(doc: &'d Doc<'_>) -> Vec<Step<'d>> {
    if let Shape::Scalar(..) = doc.shape(doc.root) {
        return vec![Step::Array(1, true), Step::Scalar(doc.root), Step::End];
    }
    let mut steps = Vec::new();
    let mut visits = vec![Visit::Node(doc.root)];
    while let Some(visit) = visits.pop() {
        match visit {
            Visit::End => steps.push(Step::End),
            Visit::Key(key) => steps.push(Step::Key(key)),
            Visit::Node(node) => match doc.shape(node) {
                Shape::Scalar(..) => steps.push(Step::Scalar(node)),
                Shape::Array(items) => {
                    steps.push(Step::Array(items.len(), false));
                    visits.push(Visit::End);
                    visits.extend(items.iter().rev().map(|&item| Visit::Node(item)));
                }
                Shape::Object(fields) => {
                    steps.push(Step::Object(fields.len()));
                    visits.push(Visit::End);
                    for (key, value) in fields.iter().rev() {
                        visits.push(Visit::Node(*value));
                        visits.push(Visit::Key(key));
                    }
                }
            },
        }
    }
    steps
}

/// The place of a type in the order of `enum jbvType`, which orders two values of two types.
fn rank(doc: &Doc<'_>, step: Step<'_>) -> u8 {
    match step {
        Step::Scalar(node) => match doc.shape(node) {
            Shape::Scalar(Kind::Null, _) => 0,
            Shape::Scalar(Kind::String, _) => 1,
            Shape::Scalar(Kind::Number, _) => 2,
            _ => 3,
        },
        Step::Key(_) => 1,
        Step::Array(..) => 0x10,
        Step::Object(_) | Step::End => 0x11,
    }
}

/// The order of two scalars, as `compareJsonbScalarValue` gives it.
fn scalar_order(a: &Doc<'_>, x: usize, b: &Doc<'_>, y: usize) -> Result<Ordering, TypeError> {
    Ok(match (a.shape(x), b.shape(y)) {
        (Shape::Scalar(Kind::String, Some(s)), Shape::Scalar(Kind::String, Some(t))) => {
            s.as_bytes().cmp(t.as_bytes())
        }
        (Shape::Scalar(Kind::Number, _), Shape::Scalar(Kind::Number, _)) => {
            numeric_in(a.span(x), -1)?.compare(&numeric_in(b.span(y), -1)?)
        }
        (
            Shape::Scalar(Kind::True | Kind::False, _),
            Shape::Scalar(Kind::True | Kind::False, _),
        ) => a.span(x).cmp(b.span(y)),
        _ => Ordering::Equal,
    })
}

/// `jsonb_cmp`: the order of two `jsonb` values, as `compareJsonbContainers` gives it.
///
/// # Errors
///
/// If a value is not the text of a `jsonb` value.
pub fn jsonb_cmp(a: &str, b: &str) -> Result<Ordering, TypeError> {
    if a == b {
        return Ok(Ordering::Equal);
    }
    let (left, right) = (Doc::read(a)?, Doc::read(b)?);
    for (&x, &y) in walk(&left).iter().zip(&walk(&right)) {
        let order = match (x, y) {
            (Step::End, Step::End) => Ordering::Equal,
            // An array that holds a raw scalar is before another array, but the number of the
            // elements decides first, so an empty array is before a scalar.
            (Step::Array(n, raw), Step::Array(m, other)) if n == m => other.cmp(&raw),
            (Step::Array(n, _), Step::Array(m, _)) | (Step::Object(n), Step::Object(m)) => {
                n.cmp(&m)
            }
            (Step::Key(s), Step::Key(t)) => s.as_bytes().cmp(t.as_bytes()),
            (Step::Scalar(p), Step::Scalar(q)) => match rank(&left, x).cmp(&rank(&right, y)) {
                Ordering::Equal => scalar_order(&left, p, &right, q)?,
                order => order,
            },
            _ => rank(&left, x).cmp(&rank(&right, y)),
        };
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}

/// The error of `cannotCastJsonbValue`.
fn cannot_cast(kind: &str, type_name: &str) -> TypeError {
    TypeError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("cannot cast jsonb {kind} to type {type_name}"),
    )
}

/// The scalar of a `jsonb` value that a cast to `type_name` reads, as `JsonbExtractScalar` finds
/// it, with its text.
fn scalar<'a>(text: &'a str, type_name: &str) -> Result<(Kind, &'a str), TypeError> {
    let doc = Doc::read(text)?;
    match doc.shape(doc.root) {
        Shape::Scalar(kind, _) => Ok((*kind, doc.span(doc.root))),
        Shape::Array(_) => Err(cannot_cast("array", type_name)),
        Shape::Object(_) => Err(cannot_cast("object", type_name)),
    }
}

/// The name of the kind of a scalar in the error of a cast.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Null => "null",
        Kind::String => "string",
        Kind::Number => "numeric",
        _ => "boolean",
    }
}

/// `jsonb_bool`: the `boolean` of a `jsonb` value, with `None` for `null`.
///
/// # Errors
///
/// If the value is not a boolean or `null`.
pub fn jsonb_bool(text: &str) -> Result<Option<bool>, TypeError> {
    match scalar(text, "boolean")? {
        (Kind::Null, _) => Ok(None),
        (Kind::True, _) => Ok(Some(true)),
        (Kind::False, _) => Ok(Some(false)),
        (kind, _) => Err(cannot_cast(kind_name(kind), "boolean")),
    }
}

/// The number of a `jsonb` value that a cast to the numeric type `type_name` reads, as
/// `jsonb_numeric`, `jsonb_int4` and the others read it, with `None` for `null`.
///
/// # Errors
///
/// If the value is not a number or `null`.
pub fn jsonb_numeric(text: &str, type_name: &str) -> Result<Option<Numeric>, TypeError> {
    match scalar(text, type_name)? {
        (Kind::Null, _) => Ok(None),
        (Kind::Number, span) => numeric_in(span, -1).map(Some),
        (kind, _) => Err(cannot_cast(kind_name(kind), type_name)),
    }
}

/// `jsonb_int2`, `jsonb_int4` and `jsonb_int8`: the number of a `jsonb` value rounded half away
/// from zero, as `numeric_int4` and the others round it, with `None` for `null`.
///
/// # Errors
///
/// If the value is not a number or `null`, or if the integer is not of the type `T`, which
/// `type_name` names.
pub fn jsonb_integer<T: TryFrom<i128>>(
    text: &str,
    type_name: &str,
) -> Result<Option<T>, TypeError> {
    let Some(number) = jsonb_numeric(text, type_name)? else { return Ok(None) };
    let range = || {
        TypeError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("{type_name} out of range"))
    };
    T::try_from(number.to_integer(type_name)?).map(Some).map_err(|_| range())
}

/// `jsonb_float4` and `jsonb_float8`: the float of the text of the number of a `jsonb` value, as
/// `numeric_float8` reads it with `float8in`, with `None` for `null`.
///
/// # Errors
///
/// If the value is not a number or `null`, or if the float is out of the range of `real`, or of
/// `double precision` when `double` is set.
pub fn jsonb_float(text: &str, double: bool) -> Result<Option<f64>, TypeError> {
    let type_name = if double { "double precision" } else { "real" };
    let Some(number) = jsonb_numeric(text, type_name)? else { return Ok(None) };
    let mut out = Vec::new();
    numeric_out(&number, &mut out);
    let text = String::from_utf8(out).unwrap_or_default();
    match double {
        true => float8_in(&text).map(Some),
        false => float4_in(&text).map(|value| Some(f64::from(value))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(a: &str, b: &str) -> Ordering {
        jsonb_cmp(a, b).unwrap()
    }

    #[test]
    fn the_types_are_in_the_order_of_postgresql() {
        let sorted = ["null", r#""a""#, "1", "false", "true", "[]", "[1]", "{}", r#"{"a": 1}"#];
        for (at, a) in sorted.iter().enumerate() {
            for (other, b) in sorted.iter().enumerate() {
                let expected = match (a, b) {
                    // An empty array is before a scalar, as in PostgreSQL.
                    (&"[]", _) if other < 5 => Ordering::Less,
                    (_, &"[]") if at < 5 => Ordering::Greater,
                    _ => at.cmp(&other),
                };
                assert_eq!(order(a, b), expected, "{a} {b}");
            }
        }
    }

    #[test]
    fn numbers_compare_by_value_and_containers_by_size_first() {
        assert_eq!(order("1.0", "1"), Ordering::Equal);
        assert_eq!(order(r#"{"a": 1.50}"#, r#"{"a": 1.5}"#), Ordering::Equal);
        assert_eq!(order("[1, 2]", "[3]"), Ordering::Greater);
        assert_eq!(order(r#"{"b": 1}"#, r#"{"a": 2}"#), Ordering::Greater);
        assert_eq!(order(r#"{"a": [1]}"#, r#"{"a": 1}"#), Ordering::Greater);
        assert_eq!(order(r#""b""#, r#""ab""#), Ordering::Greater);
        assert_eq!(order("[1]", "1"), Ordering::Greater);
    }

    #[test]
    fn a_deep_value_is_safe() {
        let deep = |end: &str| format!("{}{end}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(order(&deep("1"), &deep("2")), Ordering::Less);
    }

    #[test]
    fn a_cast_takes_only_the_scalar_of_its_kind() {
        assert_eq!(jsonb_bool("true").unwrap(), Some(true));
        assert_eq!(jsonb_bool("null").unwrap(), None);
        let error = jsonb_bool("1").unwrap_err();
        assert_eq!(error.message, "cannot cast jsonb numeric to type boolean");
        assert_eq!(error.sqlstate, SqlState::INVALID_PARAMETER_VALUE);
        let number = jsonb_numeric("1.50", "numeric").unwrap().unwrap();
        assert_eq!(number.compare(&Numeric::from_decimal(15, 1)), Ordering::Equal);
        let error = jsonb_numeric("[1]", "integer").unwrap_err();
        assert_eq!(error.message, "cannot cast jsonb array to type integer");
        let error = jsonb_float(r#""1""#, true).unwrap_err();
        assert_eq!(error.message, "cannot cast jsonb string to type double precision");
        assert_eq!(jsonb_integer::<i32>("-1.5", "integer").unwrap(), Some(-2));
        assert_eq!(jsonb_integer::<i64>("2.5", "bigint").unwrap(), Some(3));
        let error = jsonb_integer::<i16>("70000", "smallint").unwrap_err();
        assert_eq!(error.message, "smallint out of range");
        assert_eq!(jsonb_float("0.1", false).unwrap(), Some(f64::from(0.1f32)));
        let error = jsonb_float("1e400", true).unwrap_err();
        assert_eq!(error.sqlstate, SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    }
}
