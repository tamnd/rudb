//! The operators and the functions of `jsonb` that test a value or give a changed value: `?`,
//! `?|`, `?&`, `@>`, `<@`, `||`, `-`, `#-`, `jsonb_set`, `jsonb_set_lax`, `jsonb_insert` and
//! `jsonb_pretty`.
//!
//! This is a port of the functions of `src/backend/utils/adt/jsonfuncs.c` and of
//! `JsonbDeepContains` in `jsonb_util.c`. rudb keeps a `jsonb` value as the text of its normal
//! form, so each part of a value that [`Doc`] reads is in the normal form too. A function that
//! changes a value writes the new text of the one array or object that changes, and copies the
//! text of the rest. The fields of an object stay in the [`key_order`] of their keys.
//!
//! None of these functions takes recursion, so a value nested to any depth is safe.

use std::cmp::Ordering;

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::json::Kind;
use crate::jsonb::{escape_json, key_order};
use crate::jsonfuncs::{Doc, Shape, index_of, invalid};
use crate::numeric::numeric_in;

/// `jsonb_exists`, the `?` operator: whether `key` is a key of the outer object, a string element
/// of the outer array, or the string of a scalar.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn jsonb_exists(text: &str, key: &str) -> Result<bool, TypeError> {
    Ok(exists(&Doc::read(text)?, key))
}

/// `jsonb_exists_any`, the `?|` operator with `any` set, and `jsonb_exists_all`, the `?&`
/// operator without it. A null key is left out, so `?|` of no keys is false and `?&` of no keys
/// is true.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn jsonb_exists_keys(text: &str, keys: &[Option<&str>], any: bool) -> Result<bool, TypeError> {
    let doc = Doc::read(text)?;
    let mut keys = keys.iter().flatten();
    Ok(if any { keys.any(|key| exists(&doc, key)) } else { keys.all(|key| exists(&doc, key)) })
}

fn exists(doc: &Doc<'_>, key: &str) -> bool {
    match doc.shape(doc.root) {
        Shape::Object(_) => doc.field(doc.root, key).is_some(),
        Shape::Array(items) => items.iter().any(|&item| string_is(doc, item, key)),
        Shape::Scalar(..) => string_is(doc, doc.root, key),
    }
}

fn string_is(doc: &Doc<'_>, node: usize, key: &str) -> bool {
    matches!(doc.shape(node), Shape::Scalar(Kind::String, Some(text)) if text == key)
}

/// `jsonb_contains`, the `@>` operator: whether `text` contains `other`. `<@` is the same test
/// with the operands the other way.
///
/// An object contains an object whose every key it has, with a value that contains the value of
/// the key. An array contains an array whose every element it contains, in any order and any
/// number of times. A scalar is an array of one element here, but an array is not contained in a
/// scalar. Two numbers are equal by their value, so `[1.0]` contains `[1]`.
///
/// # Errors
///
/// A string of a value that cannot be text.
pub fn jsonb_contains(text: &str, other: &str) -> Result<bool, TypeError> {
    let (outer, inner) = (Doc::read(text)?, Doc::read(other)?);
    let (lhs, rhs) = (outer.root, inner.root);
    Ok(match (outer.shape(lhs), inner.shape(rhs)) {
        (Shape::Object(_), Shape::Object(_)) | (Shape::Array(_), Shape::Array(_)) => {
            contains(&outer, lhs, &inner, rhs)
        }
        (Shape::Array(items), Shape::Scalar(..)) => {
            items.iter().any(|&item| scalar_eq(&outer, item, &inner, rhs))
        }
        (Shape::Scalar(..), Shape::Scalar(..)) => scalar_eq(&outer, lhs, &inner, rhs),
        _ => false,
    })
}

/// What the test of containment does next for one pair of an array or an object of each side.
enum Step {
    /// The next field or element of the right side.
    Next,
    /// The test of a pair of nested values, whose answer decides the next step.
    Push(usize, usize),
    Done(bool),
}

/// `JsonbDeepContains` of `rhs` of `b` in `lhs` of `a`, two objects or two arrays. Each goal is a
/// pair of nodes, the place of the next field or element of the right one, and, for an array, the
/// next element of the left one to try for the element of the right one that is an array or an
/// object.
fn contains(a: &Doc<'_>, lhs: usize, b: &Doc<'_>, rhs: usize) -> bool {
    struct Goal {
        lhs: usize,
        rhs: usize,
        at: usize,
        next: usize,
    }
    let mut goals = vec![Goal { lhs, rhs, at: 0, next: 0 }];
    // The answer of the goal that ended last, for the goal under it.
    let mut answer = None;
    while let Some(goal) = goals.last_mut() {
        match answer.take() {
            Some(true) => {
                goal.at += 1;
                goal.next = 0;
            }
            Some(false) if matches!(b.shape(goal.rhs), Shape::Array(_)) => goal.next += 1,
            Some(false) => {
                goals.pop();
                answer = Some(false);
                continue;
            }
            None => {}
        }
        let step = match (a.shape(goal.lhs), b.shape(goal.rhs)) {
            (Shape::Object(outer), Shape::Object(inner)) => {
                if goal.at == 0 && outer.len() < inner.len() {
                    Step::Done(false)
                } else if let Some((key, value)) = inner.get(goal.at) {
                    match a.field(goal.lhs, key) {
                        Some(found) => pair(a, found, b, *value),
                        None => Step::Done(false),
                    }
                } else {
                    Step::Done(true)
                }
            }
            (Shape::Array(outer), Shape::Array(inner)) => match inner.get(goal.at) {
                None => Step::Done(true),
                Some(&item) if matches!(b.shape(item), Shape::Scalar(..)) => {
                    match outer.iter().any(|&other| scalar_eq(a, other, b, item)) {
                        true => Step::Next,
                        false => Step::Done(false),
                    }
                }
                Some(&item) => {
                    let tried = outer.get(goal.next..).unwrap_or_default();
                    match tried.iter().position(|&other| same_kind(a, other, b, item)) {
                        Some(skip) => {
                            goal.next += skip;
                            Step::Push(outer[goal.next], item)
                        }
                        None => Step::Done(false),
                    }
                }
            },
            _ => Step::Done(false),
        };
        match step {
            Step::Next => goal.at += 1,
            Step::Push(lhs, rhs) => goals.push(Goal { lhs, rhs, at: 0, next: 0 }),
            Step::Done(got) => {
                goals.pop();
                answer = Some(got);
            }
        }
    }
    answer.unwrap_or(true)
}

/// The step for the value `lhs` of a key of the left object and the value `rhs` of the key in the
/// right one.
fn pair(a: &Doc<'_>, lhs: usize, b: &Doc<'_>, rhs: usize) -> Step {
    match (a.shape(lhs), b.shape(rhs)) {
        (Shape::Scalar(..), Shape::Scalar(..)) => match scalar_eq(a, lhs, b, rhs) {
            true => Step::Next,
            false => Step::Done(false),
        },
        _ if same_kind(a, lhs, b, rhs) => Step::Push(lhs, rhs),
        _ => Step::Done(false),
    }
}

/// Whether `lhs` and `rhs` are two arrays or two objects.
fn same_kind(a: &Doc<'_>, lhs: usize, b: &Doc<'_>, rhs: usize) -> bool {
    matches!(
        (a.shape(lhs), b.shape(rhs)),
        (Shape::Array(_), Shape::Array(_)) | (Shape::Object(_), Shape::Object(_))
    )
}

/// `equalsJsonbScalarValue`: two scalars of one kind with the same value. Two numbers compare as
/// `numeric`.
fn scalar_eq(a: &Doc<'_>, lhs: usize, b: &Doc<'_>, rhs: usize) -> bool {
    let (Shape::Scalar(left, left_text), Shape::Scalar(right, right_text)) =
        (a.shape(lhs), b.shape(rhs))
    else {
        return false;
    };
    match (left, right) {
        (Kind::String, Kind::String) => left_text == right_text,
        (Kind::Number, Kind::Number) => {
            match (numeric_in(a.span(lhs), -1), numeric_in(b.span(rhs), -1)) {
                (Ok(x), Ok(y)) => x.compare(&y) == Ordering::Equal,
                _ => false,
            }
        }
        _ => left == right,
    }
}

/// `jsonb_concat`, the `||` operator. Two objects give one object with the keys of both, and the
/// value of the right one for a key that both have. Any other two values give an array of the
/// elements of both, where an object or a scalar is one element.
///
/// # Errors
///
/// A string of a value that cannot be text.
pub fn jsonb_concat(left: &str, right: &str) -> Result<String, TypeError> {
    let (a, b) = (Doc::read(left)?, Doc::read(right)?);
    let mut out = String::with_capacity(left.len() + right.len() + 2);
    if let (Shape::Object(lhs), Shape::Object(rhs)) = (a.shape(a.root), b.shape(b.root)) {
        let mut fields = Vec::with_capacity(lhs.len() + rhs.len());
        let (mut lhs, mut rhs) = (lhs.iter().peekable(), rhs.iter().peekable());
        loop {
            let order = match (lhs.peek(), rhs.peek()) {
                (Some(x), Some(y)) => key_order(&x.0, &y.0),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => break,
            };
            if order == Ordering::Equal {
                lhs.next();
            }
            let field = match order {
                Ordering::Less => lhs.next().map(|(key, value)| (key.as_str(), a.span(*value))),
                _ => rhs.next().map(|(key, value)| (key.as_str(), b.span(*value))),
            };
            fields.extend(field);
        }
        write_object(&fields, &mut out);
    } else {
        let mut items = elements(&a);
        items.extend(elements(&b));
        write_array(&items, &mut out);
    }
    Ok(out)
}

/// The text of each element of the outer array, or of the outer value that is not an array.
fn elements<'a>(doc: &Doc<'a>) -> Vec<&'a str> {
    match doc.shape(doc.root) {
        Shape::Array(items) => items.iter().map(|&item| doc.span(item)).collect(),
        _ => vec![doc.span(doc.root)],
    }
}

/// `jsonb_delete` and `jsonb_delete_array`, the `-` operator of a `text` and of a `text[]`: the
/// value without the fields of the outer object with one of the `keys`, or without the string
/// elements of the outer array equal to one of them. A null key is left out.
///
/// # Errors
///
/// A scalar, and a string of the value that cannot be text.
pub fn jsonb_delete(text: &str, keys: &[Option<&str>]) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    let removed = |key: &str| keys.iter().flatten().any(|&other| other == key);
    let mut out = String::with_capacity(text.len());
    match doc.shape(doc.root) {
        Shape::Scalar(..) => return Err(invalid("cannot delete from scalar".into())),
        Shape::Object(fields) => {
            let kept: Vec<(&str, &str)> = fields
                .iter()
                .filter(|(key, _)| !removed(key))
                .map(|(key, value)| (key.as_str(), doc.span(*value)))
                .collect();
            write_object(&kept, &mut out);
        }
        Shape::Array(items) => {
            let kept: Vec<&str> = items
                .iter()
                .filter(|&&item| {
                    !matches!(doc.shape(item), Shape::Scalar(Kind::String, Some(key)) if removed(key))
                })
                .map(|&item| doc.span(item))
                .collect();
            write_array(&kept, &mut out);
        }
    }
    Ok(out)
}

/// `jsonb_delete_idx`, the `-` operator of an `integer`: the outer array without the element
/// `index`. A negative index counts from the end, and an index past either end changes nothing.
///
/// # Errors
///
/// A scalar, an object, and a string of the value that cannot be text.
pub fn jsonb_delete_index(text: &str, index: i32) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    let items = match doc.shape(doc.root) {
        Shape::Scalar(..) => return Err(invalid("cannot delete from scalar".into())),
        Shape::Object(_) => {
            return Err(invalid("cannot delete from object using integer index".into()));
        }
        Shape::Array(items) => items,
    };
    let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let at = if index < 0 { len + i64::from(index) } else { i64::from(index) };
    let Some(at) = usize::try_from(at).ok().filter(|&at| at < items.len()) else {
        return Ok(text.to_owned());
    };
    let mut kept: Vec<&str> = items.iter().map(|&item| doc.span(item)).collect();
    kept.remove(at);
    let mut out = String::with_capacity(text.len());
    write_array(&kept, &mut out);
    Ok(out)
}

/// The change that `setPath` makes at the end of a path, by the `op_type` of its caller.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    /// `jsonb_set` with `create_if_missing` false: only a value that is there changes.
    Replace,
    /// `jsonb_set`: a value that is not there is added.
    Create,
    /// `jsonb_insert`, before the element or after it.
    Before,
    After,
    /// `jsonb_delete_path`, the `#-` operator.
    Delete,
}

/// `jsonb_set`: the value with `value` at the end of `path`. A key that the outer object of the
/// last step does not have is added with `create` set, and so is an index past the end of an
/// array, at the end, or before the start, at the start. A path that stops at a scalar or at a
/// missing key or index before its last step changes nothing.
///
/// # Errors
///
/// A scalar, a null step, a step into an array that is not an integer, and a string of the value
/// that cannot be text.
pub fn jsonb_set(
    text: &str,
    path: &[Option<&str>],
    value: &str,
    create: bool,
) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    match doc.shape(doc.root) {
        Shape::Scalar(..) => Err(invalid("cannot set path in scalar".into())),
        Shape::Array(items) if items.is_empty() && !create => Ok(text.to_owned()),
        Shape::Object(fields) if fields.is_empty() && !create => Ok(text.to_owned()),
        _ => change_path(&doc, path, value, if create { Change::Create } else { Change::Replace }),
    }
}

/// `jsonb_insert`: the value with `value` before the element at the end of `path`, or after it
/// with `after` set. A key that the object of the last step has is an error.
///
/// # Errors
///
/// A scalar, an existing key, a null step, a step into an array that is not an integer, and a
/// string of the value that cannot be text.
pub fn jsonb_insert(
    text: &str,
    path: &[Option<&str>],
    value: &str,
    after: bool,
) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    if let Shape::Scalar(..) = doc.shape(doc.root) {
        return Err(invalid("cannot set path in scalar".into()));
    }
    change_path(&doc, path, value, if after { Change::After } else { Change::Before })
}

/// `jsonb_delete_path`, the `#-` operator: the value without the field or the element at the end
/// of `path`.
///
/// # Errors
///
/// A scalar, a null step, a step into an array that is not an integer, and a string of the value
/// that cannot be text.
pub fn jsonb_delete_path(text: &str, path: &[Option<&str>]) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    match doc.shape(doc.root) {
        Shape::Scalar(..) => Err(invalid("cannot delete path in scalar".into())),
        Shape::Array(items) if items.is_empty() => Ok(text.to_owned()),
        Shape::Object(fields) if fields.is_empty() => Ok(text.to_owned()),
        _ => change_path(&doc, path, "", Change::Delete),
    }
}

/// `jsonb_set_lax`: `jsonb_set`, and for a null `value` the `treatment` that the caller names. A
/// null `target`, `path` or `create` gives a null.
///
/// # Errors
///
/// A null or an unknown `treatment`, the treatment `raise_exception`, and the errors of
/// [`jsonb_set`] and [`jsonb_delete_path`].
pub fn jsonb_set_lax(
    target: Option<&str>,
    path: Option<&[Option<&str>]>,
    value: Option<&str>,
    create: Option<bool>,
    treatment: Option<&str>,
) -> Result<Option<String>, TypeError> {
    let (Some(target), Some(path), Some(create)) = (target, path, create) else {
        return Ok(None);
    };
    let unknown = || {
        invalid(
            "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", \
             or \"raise_exception\""
                .into(),
        )
    };
    let treatment = treatment.ok_or_else(unknown)?;
    if let Some(value) = value {
        return jsonb_set(target, path, value, create).map(Some);
    }
    match treatment {
        "raise_exception" => {
            let mut error = TypeError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "JSON value must not be null".into(),
            );
            error.detail = Some(
                "Exception was raised because null_value_treatment is \"raise_exception\".".into(),
            );
            error.hint = Some(
                "To avoid, either change the null_value_treatment argument or ensure that an SQL \
                 NULL is not passed."
                    .into(),
            );
            Err(error)
        }
        "use_json_null" => jsonb_set(target, path, "null", create).map(Some),
        "delete_key" => jsonb_delete_path(target, path).map(Some),
        "return_target" => Ok(Some(target.to_owned())),
        _ => Err(unknown()),
    }
}

/// `jsonb_set_element`, the assignment to a subscript, `SET value['a'][0] = new`: the value with
/// `new` at the end of `path`, as `setPath` makes it with `JB_PATH_CREATE`, `JB_PATH_FILL_GAPS`
/// and `JB_PATH_CONSISTENT_POSITION`.
///
/// A missing key or index is added, and so is the rest of the path after it, as an object for a
/// step that is not an integer and as an array for one that is. An index past the end of an array
/// adds nulls before the new element, and a negative index counts from the end.
///
/// # Errors
///
/// A step into a scalar, a step into an array that is not an integer, and a negative index before
/// the start of an array.
pub fn jsonb_set_element(text: &str, path: &[&str], new: &str) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    let mut node = doc.root;
    for (level, &step) in path.iter().enumerate() {
        let position = level + 1;
        let rest = &path[position..];
        let mut out = String::new();
        match doc.shape(node) {
            Shape::Scalar(..) => {
                let mut error = invalid("cannot replace existing key".into());
                error.detail = Some(
                    "The path assumes key is a composite object, but it is a scalar value.".into(),
                );
                return Err(error);
            }
            Shape::Object(fields) => {
                let found = fields.iter().position(|(key, _)| key == step);
                if let Some(at) = found
                    && !rest.is_empty()
                {
                    node = fields[at].1;
                    continue;
                }
                let mut kept: Vec<(&str, &str)> =
                    fields.iter().map(|(key, value)| (key.as_str(), doc.span(*value))).collect();
                let added = new_path(rest, new);
                match found {
                    Some(at) => kept[at].1 = new,
                    None => {
                        let at = kept.partition_point(|(key, _)| key_order(key, step).is_lt());
                        kept.insert(at, (step, &added));
                    }
                }
                write_object(&kept, &mut out);
            }
            Shape::Array(items) => {
                let index = index_of(step).ok_or_else(|| {
                    TypeError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        format!(
                            "path element at position {position} is not an integer: \"{step}\""
                        ),
                    )
                })?;
                let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
                if index < 0 && -index > len {
                    return Err(invalid(format!(
                        "path element at position {position} is out of range: {index}"
                    )));
                }
                let at = if index < 0 { len + index } else { index };
                let found = usize::try_from(at).ok().filter(|&at| at < items.len());
                if let Some(at) = found
                    && !rest.is_empty()
                {
                    node = items[at];
                    continue;
                }
                let mut kept: Vec<&str> = items.iter().map(|&item| doc.span(item)).collect();
                let added = new_path(rest, new);
                match found {
                    Some(at) => kept[at] = new,
                    None => {
                        let gap = usize::try_from(at - len).unwrap_or_default();
                        kept.extend(std::iter::repeat_n("null", gap));
                        kept.push(&added);
                    }
                }
                write_array(&kept, &mut out);
            }
        }
        let (start, end) = doc.bounds(node);
        return Ok([&text[..start], &out, &text[end..]].concat());
    }
    Ok(text.to_owned())
}

/// `push_path`: `new` at the end of `path` in new objects and arrays, an object for a step that
/// is not an integer and an array for one that is. An array has as many nulls before the next
/// step as the step says.
fn new_path(path: &[&str], new: &str) -> String {
    let mut out = new.to_owned();
    for &step in path.iter().rev() {
        let mut outer = String::with_capacity(out.len() + step.len() + 4);
        match index_of(step) {
            Some(index) => {
                outer.push('[');
                for _ in 0..index.max(0) {
                    outer.push_str("null, ");
                }
                outer.push_str(&out);
                outer.push(']');
            }
            None => {
                outer.push('{');
                escape_json(step, &mut outer);
                outer.push_str(": ");
                outer.push_str(&out);
                outer.push('}');
            }
        }
        out = outer;
    }
    out
}

/// `setPath`: the value with `change` at the end of `path`. Each step reads the key of an object
/// or the index of an array, which `strtoint` reads. The text of the one array or object that
/// changes is written again, and the text around it is copied.
fn change_path(
    doc: &Doc<'_>,
    path: &[Option<&str>],
    value: &str,
    change: Change,
) -> Result<String, TypeError> {
    let mut node = doc.root;
    for (level, step) in path.iter().enumerate() {
        let position = level + 1;
        let Some(step) = *step else {
            return Err(TypeError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!("path element at position {position} is null"),
            ));
        };
        let last = position == path.len();
        let mut out = String::new();
        match doc.shape(node) {
            Shape::Scalar(..) => break,
            Shape::Object(fields) => {
                let found = fields.iter().position(|(key, _)| key == step);
                if !last {
                    match found {
                        Some(at) => {
                            node = fields[at].1;
                            continue;
                        }
                        None => break,
                    }
                }
                let mut kept: Vec<(&str, &str)> =
                    fields.iter().map(|(key, value)| (key.as_str(), doc.span(*value))).collect();
                match (found, change) {
                    (Some(_), Change::Before | Change::After) => {
                        let mut error = invalid("cannot replace existing key".into());
                        error.hint =
                            Some("Try using the function jsonb_set to replace key value.".into());
                        return Err(error);
                    }
                    (Some(at), Change::Delete) => {
                        kept.remove(at);
                    }
                    (Some(at), _) => kept[at].1 = value,
                    (None, Change::Replace | Change::Delete) => break,
                    (None, _) => {
                        let at = kept.partition_point(|(key, _)| key_order(key, step).is_lt());
                        kept.insert(at, (step, value));
                    }
                }
                write_object(&kept, &mut out);
            }
            Shape::Array(items) => {
                let index = index_of(step).ok_or_else(|| {
                    TypeError::new(
                        SqlState::INVALID_TEXT_REPRESENTATION,
                        format!(
                            "path element at position {position} is not an integer: \"{step}\""
                        ),
                    )
                })?;
                let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
                // `None` for a negative index before the start.
                let at =
                    if index < 0 { (len + index >= 0).then_some(len + index) } else { Some(index) };
                let found =
                    at.and_then(|at| usize::try_from(at).ok()).filter(|&at| at < items.len());
                if !last {
                    match found {
                        Some(at) => {
                            node = items[at];
                            continue;
                        }
                        None => break,
                    }
                }
                let mut kept: Vec<&str> = items.iter().map(|&item| doc.span(item)).collect();
                match (found, change) {
                    (Some(at), Change::Delete) => {
                        kept.remove(at);
                    }
                    (Some(at), Change::Replace | Change::Create) => kept[at] = value,
                    (Some(at), Change::Before) => kept.insert(at, value),
                    (Some(at), Change::After) => kept.insert(at + 1, value),
                    (None, Change::Replace | Change::Delete) => break,
                    (None, _) if at.is_none() || kept.is_empty() => kept.insert(0, value),
                    (None, _) => kept.push(value),
                }
                write_array(&kept, &mut out);
            }
        }
        let (start, end) = doc.bounds(node);
        let text = doc.text;
        return Ok([&text[..start], &out, &text[end..]].concat());
    }
    Ok(doc.text.to_owned())
}

fn write_object(fields: &[(&str, &str)], out: &mut String) {
    out.push('{');
    for (at, (key, value)) in fields.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        escape_json(key, out);
        out.push_str(": ");
        out.push_str(value);
    }
    out.push('}');
}

fn write_array(items: &[&str], out: &mut String) {
    out.push('[');
    for (at, item) in items.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(item);
    }
    out.push(']');
}

/// `jsonb_pretty`: the value with each element and each field on a line of its own, indented by
/// four spaces for each level. An empty array or object has its two brackets on two lines.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn jsonb_pretty(text: &str) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    let mut out = String::with_capacity(text.len() * 2);
    let line = |depth: usize, out: &mut String| {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', depth * 4));
    };
    // Each entry is a node and the place of its next child. The depth of a node is the number of
    // entries under it.
    let mut stack = vec![(doc.root, 0usize)];
    while let Some((node, at)) = stack.pop() {
        let depth = stack.len();
        let (open, close, len) = match doc.shape(node) {
            Shape::Scalar(..) => {
                out.push_str(doc.span(node));
                continue;
            }
            Shape::Array(items) => ('[', ']', items.len()),
            Shape::Object(fields) => ('{', '}', fields.len()),
        };
        if at == 0 {
            out.push(open);
        }
        if at == len {
            line(depth, &mut out);
            out.push(close);
            continue;
        }
        if at > 0 {
            out.push(',');
        }
        line(depth + 1, &mut out);
        let child = match doc.shape(node) {
            Shape::Object(fields) => {
                escape_json(&fields[at].0, &mut out);
                out.push_str(": ");
                fields[at].1
            }
            Shape::Array(items) => items[at],
            Shape::Scalar(..) => continue,
        };
        stack.push((node, at + 1));
        stack.push((child, 0));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::StackDepth;
    use crate::jsonb::jsonb_in;

    fn jsonb(text: &str) -> String {
        jsonb_in(text, StackDepth::default()).unwrap()
    }

    #[test]
    fn a_key_exists_only_at_the_outer_level() {
        let cases = [
            (r#"{"a":1}"#, true),
            (r#"["a",1]"#, true),
            (r#""a""#, true),
            (r#"[["a"]]"#, false),
            (r#"{"b":{"a":1}}"#, false),
        ];
        for (text, exists) in cases {
            assert_eq!(jsonb_exists(&jsonb(text), "a").unwrap(), exists, "{text}");
        }
        let doc = jsonb(r#"{"a":1,"b":2}"#);
        assert!(jsonb_exists_keys(&doc, &[Some("b"), Some("c")], true).unwrap());
        assert!(!jsonb_exists_keys(&doc, &[], true).unwrap());
        assert!(jsonb_exists_keys(&doc, &[], false).unwrap());
        assert!(jsonb_exists_keys(&doc, &[Some("a"), None], false).unwrap());
    }

    #[test]
    fn containment_follows_postgresql() {
        let cases = [
            (r#"{"a":1,"b":[1,2,{"c":3}]}"#, r#"{"b":[{"c":3}]}"#, true),
            ("[1,2]", "1", true),
            ("1", "[1]", false),
            ("[1.0]", "[1]", true),
            ("[[1,2]]", "[1]", false),
            (r#"{"a":[1,2]}"#, r#"{"a":1}"#, false),
            ("[1,2,2]", "[2,2,2]", true),
            ("[]", "[]", true),
            ("{}", "{}", true),
            ("[{}]", "{}", false),
            (r#"[[1],[2,3]]"#, r#"[[3,2]]"#, true),
            (r#"[{"a":1},{"a":2,"b":3}]"#, r#"[{"b":3,"a":2},{"a":1}]"#, true),
            (r#"[{"a":1},{"b":3}]"#, r#"[{"a":1,"b":3}]"#, false),
            ("null", "[null]", false),
        ];
        for (outer, inner, contains) in cases {
            let got = jsonb_contains(&jsonb(outer), &jsonb(inner)).unwrap();
            assert_eq!(got, contains, "{outer} @> {inner}");
        }
    }

    #[test]
    fn concat_merges_objects_and_joins_the_rest_into_arrays() {
        let cases = [
            (r#"{"a":1}"#, r#"{"a":2,"b":3}"#, r#"{"a": 2, "b": 3}"#),
            (r#"{"bb":1,"c":2}"#, r#"{"a":0,"c":3}"#, r#"{"a": 0, "c": 3, "bb": 1}"#),
            ("[1]", "[2]", "[1, 2]"),
            ("1", "2", "[1, 2]"),
            (r#"{"a":1}"#, "[1]", r#"[{"a": 1}, 1]"#),
            ("[1]", r#"{"a":1}"#, r#"[1, {"a": 1}]"#),
            ("[]", "{}", "[{}]"),
        ];
        for (left, right, want) in cases {
            assert_eq!(jsonb_concat(&jsonb(left), &jsonb(right)).unwrap(), want);
        }
    }

    #[test]
    fn delete_takes_keys_strings_and_indexes() {
        let doc = jsonb(r#"{"a":1,"b":2}"#);
        assert_eq!(jsonb_delete(&doc, &[Some("a")]).unwrap(), r#"{"b": 2}"#);
        assert_eq!(jsonb_delete(&doc, &[Some("a"), None, Some("b")]).unwrap(), "{}");
        let doc = jsonb(r#"["a","b","a",1,null]"#);
        assert_eq!(jsonb_delete(&doc, &[Some("a"), Some("1")]).unwrap(), r#"["b", 1, null]"#);
        let error = jsonb_delete("1", &[Some("a")]).unwrap_err();
        assert_eq!(error.message, "cannot delete from scalar");
        assert_eq!(jsonb_delete_index("[1, 2, 3]", 1).unwrap(), "[1, 3]");
        assert_eq!(jsonb_delete_index("[1, 2, 3]", -1).unwrap(), "[1, 2]");
        assert_eq!(jsonb_delete_index("[1, 2, 3]", 5).unwrap(), "[1, 2, 3]");
        assert_eq!(jsonb_delete_index("[1, 2, 3]", -4).unwrap(), "[1, 2, 3]");
        let error = jsonb_delete_index(r#"{"a": 1}"#, 0).unwrap_err();
        assert_eq!(error.message, "cannot delete from object using integer index");
    }

    #[test]
    fn a_path_sets_inserts_and_deletes() {
        let doc = jsonb(r#"{"a":[1,2]}"#);
        let set = |path: &[&str], create| {
            let path: Vec<Option<&str>> = path.iter().copied().map(Some).collect();
            jsonb_set(&doc, &path, r#""x""#, create).unwrap()
        };
        assert_eq!(set(&["a", "0"], true), r#"{"a": ["x", 2]}"#);
        assert_eq!(set(&["a", "5"], true), r#"{"a": [1, 2, "x"]}"#);
        assert_eq!(set(&["a", "-5"], true), r#"{"a": ["x", 1, 2]}"#);
        assert_eq!(set(&["a", " -1"], true), r#"{"a": [1, "x"]}"#);
        assert_eq!(set(&["a", "5"], false), r#"{"a": [1, 2]}"#);
        assert_eq!(set(&["b"], true), r#"{"a": [1, 2], "b": "x"}"#);
        assert_eq!(set(&["0"], true), r#"{"0": "x", "a": [1, 2]}"#);
        assert_eq!(set(&["b", "c"], true), r#"{"a": [1, 2]}"#);
        assert_eq!(set(&["a", "0", "c"], true), r#"{"a": [1, 2]}"#);
        let error = jsonb_set(&doc, &[Some("a"), Some("x")], "1", true).unwrap_err();
        assert_eq!(error.message, "path element at position 2 is not an integer: \"x\"");
        assert_eq!(error.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        let error = jsonb_set(&doc, &[Some("a"), Some("1 ")], "1", true).unwrap_err();
        assert_eq!(error.message, "path element at position 2 is not an integer: \"1 \"");
        let error = jsonb_set(&doc, &[None], "1", true).unwrap_err();
        assert_eq!(error.message, "path element at position 1 is null");
        assert_eq!(jsonb_set("{}", &[None], "1", false).unwrap(), "{}");
        assert_eq!(jsonb_set("[]", &[Some("-3")], "2", true).unwrap(), "[2]");
        assert_eq!(jsonb_set(&doc, &[], "1", true).unwrap(), doc);
        assert!(jsonb_set("1", &[Some("a")], "1", true).is_err());

        let insert = |path: &[&str], after| {
            let path: Vec<Option<&str>> = path.iter().copied().map(Some).collect();
            jsonb_insert(&doc, &path, r#""x""#, after)
        };
        assert_eq!(insert(&["a", "1"], false).unwrap(), r#"{"a": [1, "x", 2]}"#);
        assert_eq!(insert(&["a", "1"], true).unwrap(), r#"{"a": [1, 2, "x"]}"#);
        assert_eq!(insert(&["a", "-5"], false).unwrap(), r#"{"a": ["x", 1, 2]}"#);
        let error = insert(&["a"], false).unwrap_err();
        assert_eq!(error.message, "cannot replace existing key");

        let doc = jsonb(r#"{"a":[1,{"b":2}]}"#);
        let delete = |path: &[&str]| {
            let path: Vec<Option<&str>> = path.iter().copied().map(Some).collect();
            jsonb_delete_path(&doc, &path).unwrap()
        };
        assert_eq!(delete(&["a", "1", "b"]), r#"{"a": [1, {}]}"#);
        assert_eq!(delete(&["a", "-1"]), r#"{"a": [1]}"#);
        assert_eq!(delete(&["a"]), "{}");
        assert_eq!(delete(&["c"]), doc);
    }

    #[test]
    fn set_lax_treats_a_null_by_its_argument() {
        let doc = r#"{"a": 1}"#;
        let path = [Some("a")];
        let lax = |treatment| jsonb_set_lax(Some(doc), Some(&path), None, Some(true), treatment);
        assert_eq!(lax(Some("use_json_null")).unwrap().unwrap(), r#"{"a": null}"#);
        assert_eq!(lax(Some("delete_key")).unwrap().unwrap(), "{}");
        assert_eq!(lax(Some("return_target")).unwrap().unwrap(), doc);
        let error = lax(Some("raise_exception")).unwrap_err();
        assert_eq!(error.sqlstate, SqlState::NULL_VALUE_NOT_ALLOWED);
        assert!(lax(Some("bogus")).is_err());
        assert!(lax(None).is_err());
        assert_eq!(jsonb_set_lax(Some(doc), Some(&path), None, None, None).unwrap(), None);
    }

    #[test]
    fn pretty_puts_each_value_on_a_line() {
        let doc = jsonb(r#"{"a":[1,{"b":[]},{}],"c":"x"}"#);
        let want = "{\n    \"a\": [\n        1,\n        {\n            \"b\": [\n            ]\n        },\n        {\n        }\n    ],\n    \"c\": \"x\"\n}";
        assert_eq!(jsonb_pretty(&doc).unwrap(), want);
        assert_eq!(jsonb_pretty("[]").unwrap(), "[\n]");
        assert_eq!(jsonb_pretty("1").unwrap(), "1");
    }

    #[test]
    fn a_subscript_assignment_fills_the_path_it_takes() {
        let set = |doc: &str, path: &[&str], new: &str| jsonb_set_element(doc, path, new);
        assert_eq!(set("{}", &["a"], "1").unwrap(), r#"{"a": 1}"#);
        assert_eq!(set("[]", &["0"], "1").unwrap(), "[1]");
        assert_eq!(set("[1]", &["5"], "2").unwrap(), "[1, null, null, null, null, 2]");
        assert_eq!(
            set("[1, null, null, null, null, 2]", &["-4"], "3").unwrap(),
            "[1, null, 3, null, null, 2]"
        );
        let error = set("[1, 2]", &["-8"], "1").unwrap_err();
        assert_eq!(error.message, "path element at position 1 is out of range: -8");
        let deep = set("{}", &["a", "0", "b", "0", "c"], "1").unwrap();
        assert_eq!(deep, r#"{"a": [{"b": [{"c": 1}]}]}"#);
        assert_eq!(
            set(&deep, &["a", "2", "b", "2", "c", "2"], "1").unwrap(),
            r#"{"a": [{"b": [{"c": 1}]}, null, {"b": [null, null, {"c": [null, null, 1]}]}]}"#
        );
        let doc = r#"{"a": [1, {"b": 2}]}"#;
        assert_eq!(
            set(doc, &["a", "1", "c"], r#""x""#).unwrap(),
            r#"{"a": [1, {"b": 2, "c": "x"}]}"#
        );
        assert_eq!(set(doc, &["a", "3"], "null").unwrap(), r#"{"a": [1, {"b": 2}, null, null]}"#);
        for (doc, path) in [(r#"{"a": 1}"#, &["a", "b"][..]), ("null", &["0"][..])] {
            let error = set(doc, path, "1").unwrap_err();
            assert_eq!(error.message, "cannot replace existing key");
            assert!(error.detail.unwrap().starts_with("The path assumes key is a composite"));
        }
        let error = set("[1]", &["x"], "1").unwrap_err();
        assert_eq!(error.message, "path element at position 1 is not an integer: \"x\"");
    }

    #[test]
    fn a_deep_value_takes_no_recursion() {
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(jsonb_contains(&deep, &deep).unwrap());
        assert_eq!(jsonb_pretty(&deep).unwrap().lines().count(), 20_000);
    }
}
