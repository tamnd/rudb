//! The functions that read a `json` or a `jsonb` value: `->`, `->>`, `#>`, `#>>`,
//! `json_array_length`, `json_typeof`, `json_strip_nulls`, `json_object_keys`,
//! `json_array_elements`, `json_each` and the `jsonb` form of each.
//!
//! This is a port of the functions of `src/backend/utils/adt/jsonfuncs.c`. PostgreSQL reads a
//! `json` value with the parser and gives the part of the text that a value takes, so `->` of a
//! `json` value keeps the spaces of the input inside the value. rudb keeps a `jsonb` value as the
//! text of its normal form, so the same reader gives the normal form of each part of it. Each
//! node of [`Doc`] keeps the place of its text in the input for that reason.
//!
//! PostgreSQL reads the strings with their escapes decoded, so a key compares as text. A string
//! with `\u0000` cannot be text, so it stops the read with the error of `jsonb`, as in PostgreSQL.

use rudb_common::SqlState;

use crate::error::TypeError;
use crate::json::{self, Kind, Open, Sink, StackDepth, Stop};
use crate::jsonb::escape_json;

pub(crate) enum Shape {
    /// A scalar, with the decoded text of a string.
    Scalar(Kind, Option<String>),
    Array(Vec<usize>),
    /// The fields, each a decoded key and the node of its value. A `json` value can have two
    /// fields with one key.
    Object(Vec<(String, usize)>),
}

struct Node {
    shape: Shape,
    /// The bytes of the value in the input.
    start: usize,
    end: usize,
}

/// A `json` or a `jsonb` value, read into one vector of nodes that refer to each other by index.
pub(crate) struct Doc<'a> {
    pub(crate) text: &'a str,
    nodes: Vec<Node>,
    pub(crate) root: usize,
}

#[derive(Default)]
struct Builder {
    nodes: Vec<Node>,
    /// The open arrays and objects, innermost last, each with the key of its next field.
    open: Vec<(usize, Option<String>)>,
    root: Option<usize>,
}

impl Builder {
    /// Puts a finished node into the array or the object around it.
    fn attach(&mut self, node: usize) {
        let Some((parent, key)) = self.open.last_mut() else {
            self.root = Some(node);
            return;
        };
        match &mut self.nodes[*parent].shape {
            Shape::Array(items) => items.push(node),
            Shape::Object(fields) => fields.push((key.take().unwrap_or_default(), node)),
            Shape::Scalar(..) => {}
        }
    }
}

impl Sink for Builder {
    fn open(&mut self, open: Open, start: usize) {
        let node = self.nodes.len();
        let shape = match open {
            Open::Array => Shape::Array(Vec::new()),
            Open::Object => Shape::Object(Vec::new()),
        };
        self.nodes.push(Node { shape, start, end: start });
        self.open.push((node, None));
    }

    fn close(&mut self, end: usize) {
        let Some((node, _)) = self.open.pop() else { return };
        self.nodes[node].end = end;
        self.attach(node);
    }

    fn key(&mut self, key: &str) {
        if let Some((_, slot)) = self.open.last_mut() {
            *slot = Some(key.to_owned());
        }
    }

    fn scalar(
        &mut self,
        kind: Kind,
        start: usize,
        token: &str,
        text: &str,
    ) -> Result<(), TypeError> {
        let decoded = (kind == Kind::String).then(|| text.to_owned());
        let node = self.nodes.len();
        self.nodes.push(Node {
            shape: Shape::Scalar(kind, decoded),
            start,
            end: start + token.len(),
        });
        self.attach(node);
        Ok(())
    }
}

impl<'a> Doc<'a> {
    /// Reads `text`, with the strings decoded, as `makeJsonLexContext` with `need_escapes` set.
    pub(crate) fn read(text: &'a str) -> Result<Doc<'a>, TypeError> {
        let mut builder = Builder::default();
        match json::parse(text, true, StackDepth::default(), &mut builder) {
            Ok(()) => {}
            Err(Stop::Syntax(fail)) => return Err(json::syntax(text, fail)),
            Err(Stop::Action(error)) => return Err(error),
        }
        let root = builder.root.unwrap_or_default();
        Ok(Doc { text, nodes: builder.nodes, root })
    }

    pub(crate) fn shape(&self, node: usize) -> &Shape {
        &self.nodes[node].shape
    }

    /// The bytes of the value of `node` in the input.
    pub(crate) fn bounds(&self, node: usize) -> (usize, usize) {
        let node = &self.nodes[node];
        (node.start, node.end)
    }

    /// The text of the value of `node` in the input.
    pub(crate) fn span(&self, node: usize) -> &'a str {
        let (start, end) = self.bounds(node);
        &self.text[start..end]
    }

    /// The value of the field `key` of the object `node`. Of two fields with one key, the last
    /// one is the value, as the parse of `get_worker` keeps the last match.
    pub(crate) fn field(&self, node: usize, key: &str) -> Option<usize> {
        let Shape::Object(fields) = self.shape(node) else { return None };
        fields.iter().rev().find(|(name, _)| name == key).map(|&(_, value)| value)
    }

    /// The element `index` of the array `node`. A negative index counts from the end.
    fn element(&self, node: usize, index: i64) -> Option<usize> {
        let Shape::Array(items) = self.shape(node) else { return None };
        let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let index = if index < 0 { len + index } else { index };
        usize::try_from(index).ok().and_then(|index| items.get(index).copied())
    }

    /// The value of `node` as `json`, or as `text` with `as_text` set: the decoded text of a
    /// string, a null for the JSON `null` and the text of any other value.
    fn value(&self, node: usize, as_text: bool) -> Option<String> {
        match self.shape(node) {
            Shape::Scalar(Kind::String, Some(text)) if as_text => Some(text.clone()),
            Shape::Scalar(Kind::Null, _) if as_text => None,
            _ => Some(self.span(node).to_owned()),
        }
    }

    /// The text of the value with the fields whose value is `null` left out, and the elements
    /// that are `null` too with `in_arrays` set. `colon` and `comma` are the separators of the
    /// output. A value nested to any depth takes no recursion.
    fn strip_nulls(&self, in_arrays: bool, (colon, comma): (&str, &str), out: &mut String) {
        let null = |node: usize| matches!(self.shape(node), Shape::Scalar(Kind::Null, _));
        // The open arrays and objects, each with the place of its next child and whether it has
        // written a child yet.
        let mut stack: Vec<(usize, usize, bool)> = Vec::new();
        let mut next = Some(self.root);
        loop {
            if let Some(node) = next.take() {
                match self.shape(node) {
                    Shape::Scalar(Kind::String, Some(text)) => escape_json(text, out),
                    Shape::Scalar(..) => out.push_str(self.span(node)),
                    Shape::Array(_) => {
                        out.push('[');
                        stack.push((node, 0, false));
                    }
                    Shape::Object(_) => {
                        out.push('{');
                        stack.push((node, 0, false));
                    }
                }
            }
            let Some((node, at, wrote)) = stack.last_mut() else { break };
            let child = match self.shape(*node) {
                Shape::Array(items) => {
                    let kept = items[*at..].iter().position(|&item| !(in_arrays && null(item)));
                    kept.map(|skip| (None, items[*at + skip], skip))
                }
                Shape::Object(fields) => {
                    let kept = fields[*at..].iter().position(|&(_, value)| !null(value));
                    kept.map(|skip| {
                        let (key, value) = &fields[*at + skip];
                        (Some(key), *value, skip)
                    })
                }
                Shape::Scalar(..) => None,
            };
            let Some((key, child, skip)) = child else {
                out.push(if matches!(self.shape(*node), Shape::Array(_)) { ']' } else { '}' });
                stack.pop();
                continue;
            };
            *at += skip + 1;
            if *wrote {
                out.push_str(comma);
            }
            *wrote = true;
            if let Some(key) = key {
                escape_json(key, out);
                out.push_str(colon);
            }
            next = Some(child);
        }
    }
}

/// An error of a function here, all of which PostgreSQL gives as an invalid parameter value.
pub(crate) fn invalid(message: String) -> TypeError {
    TypeError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `json_object_field` and `jsonb_object_field`, the `->` of a key, and the `_text` forms of
/// `->>` with `as_text` set. A value that is not an object, or that has no such key, gives a null.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn json_object_field(
    text: &str,
    key: &str,
    as_text: bool,
) -> Result<Option<String>, TypeError> {
    let doc = Doc::read(text)?;
    Ok(doc.field(doc.root, key).and_then(|node| doc.value(node, as_text)))
}

/// `json_array_element` and `jsonb_array_element`, the `->` of an index, and the `_text` forms of
/// `->>` with `as_text` set. A negative index counts from the end of the array.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn json_array_element(
    text: &str,
    index: i32,
    as_text: bool,
) -> Result<Option<String>, TypeError> {
    let doc = Doc::read(text)?;
    Ok(doc.element(doc.root, i64::from(index)).and_then(|node| doc.value(node, as_text)))
}

/// `json_extract_path` and `jsonb_extract_path`, the `#>` operator, and the `_text` forms of
/// `#>>` with `as_text` set. Each step of the path is the key of an object or the index of an
/// array, which `strtoint` reads, so a step that is not a whole number finds no element. A null
/// step gives a null, and an empty path gives the whole value.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn json_extract_path(
    text: &str,
    path: &[Option<&str>],
    as_text: bool,
) -> Result<Option<String>, TypeError> {
    if path.iter().any(Option::is_none) {
        return Ok(None);
    }
    let doc = Doc::read(text)?;
    let mut node = doc.root;
    for step in path.iter().flatten() {
        let next = match doc.shape(node) {
            Shape::Object(_) => doc.field(node, step),
            Shape::Array(_) => index_of(step).and_then(|index| doc.element(node, index)),
            Shape::Scalar(..) => None,
        };
        let Some(next) = next else { return Ok(None) };
        node = next;
    }
    Ok(doc.value(node, as_text))
}

/// `strtoint` of a step of a path: an integer after any white space, with an optional sign and no
/// other byte after it.
pub(crate) fn index_of(step: &str) -> Option<i64> {
    let trimmed = step.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    trimmed.parse::<i32>().ok().map(i64::from)
}

/// `json_array_length` and `jsonb_array_length`.
///
/// # Errors
///
/// A value that is not an array, and a string of the value that cannot be text.
pub fn json_array_length(text: &str) -> Result<i32, TypeError> {
    let doc = Doc::read(text)?;
    match doc.shape(doc.root) {
        Shape::Array(items) => Ok(i32::try_from(items.len()).unwrap_or(i32::MAX)),
        Shape::Object(_) => Err(invalid("cannot get array length of a non-array".into())),
        Shape::Scalar(..) => Err(invalid("cannot get array length of a scalar".into())),
    }
}

/// `json_typeof` and `jsonb_typeof`: the name of the type of the outer value.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn json_typeof(text: &str) -> Result<&'static str, TypeError> {
    let doc = Doc::read(text)?;
    Ok(match doc.shape(doc.root) {
        Shape::Object(_) => "object",
        Shape::Array(_) => "array",
        Shape::Scalar(Kind::String, _) => "string",
        Shape::Scalar(Kind::Number, _) => "number",
        Shape::Scalar(Kind::True | Kind::False, _) => "boolean",
        Shape::Scalar(..) => "null",
    })
}

/// `json_strip_nulls` and `jsonb_strip_nulls`: the value with each field whose value is `null`
/// left out, at any depth, and each element that is `null` too with `in_arrays` set. The `json`
/// form writes no space, as the semantic actions of `json_strip_nulls` do, and the `jsonb` form
/// is in the normal form.
///
/// # Errors
///
/// A string of the value that cannot be text.
pub fn json_strip_nulls(text: &str, in_arrays: bool, jsonb: bool) -> Result<String, TypeError> {
    let doc = Doc::read(text)?;
    let separators = if jsonb { (": ", ", ") } else { (":", ",") };
    let mut out = String::with_capacity(text.len());
    doc.strip_nulls(in_arrays, separators, &mut out);
    Ok(out)
}

/// `json_object_keys` and `jsonb_object_keys`, named `name` in the errors: the keys of the outer
/// object, in order. A `json` value gives a key as often as it has it.
///
/// # Errors
///
/// A value that is not an object, and a string of the value that cannot be text.
pub fn json_object_keys(text: &str, name: &str) -> Result<Vec<String>, TypeError> {
    let doc = Doc::read(text)?;
    match doc.shape(doc.root) {
        Shape::Object(fields) => Ok(fields.iter().map(|(key, _)| key.clone()).collect()),
        Shape::Array(_) => Err(invalid(format!("cannot call {name} on an array"))),
        Shape::Scalar(..) => Err(invalid(format!("cannot call {name} on a scalar"))),
    }
}

/// `json_array_elements` and `jsonb_array_elements`, named `name` in the errors, and the `_text`
/// forms with `as_text` set: the elements of the outer array.
///
/// # Errors
///
/// A value that is not an array, with the error of each function, and a string of the value that
/// cannot be text.
pub fn json_array_elements(
    text: &str,
    name: &str,
    as_text: bool,
    jsonb: bool,
) -> Result<Vec<Option<String>>, TypeError> {
    let doc = Doc::read(text)?;
    match (doc.shape(doc.root), jsonb) {
        (Shape::Array(items), _) => {
            Ok(items.iter().map(|&item| doc.value(item, as_text)).collect())
        }
        (Shape::Object(_), false) => Err(invalid(format!("cannot call {name} on a non-array"))),
        (Shape::Scalar(..), false) => Err(invalid(format!("cannot call {name} on a scalar"))),
        (Shape::Object(_), true) => Err(invalid("cannot extract elements from an object".into())),
        (Shape::Scalar(..), true) => Err(invalid("cannot extract elements from a scalar".into())),
    }
}

/// The key and the value of each field of `json_each` and `jsonb_each`, named `name` in the
/// errors, and of the `_text` forms with `as_text` set.
pub type Fields = Vec<(String, Option<String>)>;

/// `json_each` and `jsonb_each`, named `name` in the errors, and the `_text` forms with `as_text`
/// set: the fields of the outer object.
///
/// # Errors
///
/// A value that is not an object, with the error of each function, and a string of the value that
/// cannot be text.
pub fn json_each(text: &str, name: &str, as_text: bool, jsonb: bool) -> Result<Fields, TypeError> {
    let doc = Doc::read(text)?;
    match (doc.shape(doc.root), jsonb) {
        (Shape::Object(fields), _) => Ok(fields
            .iter()
            .map(|(key, value)| (key.clone(), doc.value(*value, as_text)))
            .collect()),
        (_, true) => Err(invalid(format!("cannot call {name} on a non-object"))),
        (Shape::Array(_), false) => Err(invalid("cannot deconstruct an array as an object".into())),
        (Shape::Scalar(..), false) => Err(invalid("cannot deconstruct a scalar".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_of_a_json_value_keeps_its_text() {
        let doc = r#" {"a": [1, {"b" : "xé"}], "a" : 2, "c": null} "#;
        assert_eq!(json_object_field(doc, "a", false).unwrap().as_deref(), Some("2"));
        assert_eq!(json_object_field(doc, "c", false).unwrap().as_deref(), Some("null"));
        assert_eq!(json_object_field(doc, "c", true).unwrap(), None);
        assert_eq!(json_object_field(doc, "d", false).unwrap(), None);
        assert_eq!(json_array_element(doc, 0, false).unwrap(), None);
        let arr = r#"[1, {"b" : "xé"}]"#;
        assert_eq!(json_array_element(arr, -1, false).unwrap().as_deref(), Some(r#"{"b" : "xé"}"#));
        assert_eq!(json_array_element(arr, -3, false).unwrap(), None);
        let path =
            |steps: &[Option<&str>], as_text| json_extract_path(arr, steps, as_text).unwrap();
        assert_eq!(path(&[Some("1"), Some("b")], true).as_deref(), Some("xé"));
        assert_eq!(path(&[Some(" -1"), Some("b")], false).as_deref(), Some(r#""xé""#));
        assert_eq!(path(&[Some("x")], false), None);
        assert_eq!(path(&[Some("0"), None], false), None);
        assert_eq!(path(&[], false).as_deref(), Some(arr));
    }

    #[test]
    fn the_functions_refuse_what_postgresql_refuses() {
        assert_eq!(json_array_length("[1, [2]]").unwrap(), 2);
        assert_eq!(
            json_array_length("{}").unwrap_err().message,
            "cannot get array length of a non-array"
        );
        assert_eq!(
            json_array_length("1").unwrap_err().message,
            "cannot get array length of a scalar"
        );
        let keys = json_object_keys(r#"{"a":1,"a":2}"#, "json_object_keys").unwrap();
        assert_eq!(keys, ["a", "a"]);
        let error = json_each("[1]", "json_each", false, false).unwrap_err();
        assert_eq!(error.message, "cannot deconstruct an array as an object");
        let error = json_each("[1]", "jsonb_each", false, true).unwrap_err();
        assert_eq!(error.message, "cannot call jsonb_each on a non-object");
        let error = json_object_field(r#"{"a":"\u0000"}"#, "a", false).unwrap_err();
        assert_eq!(error.message, "unsupported Unicode escape sequence");
    }

    #[test]
    fn strip_nulls_writes_the_value_again() {
        let doc = r#"{"a" : null, "b" : [1, null, {"c":null, "d" : "é"}] , "e":"x\"y"}"#;
        assert_eq!(
            json_strip_nulls(doc, false, false).unwrap(),
            r#"{"b":[1,null,{"d":"é"}],"e":"x\"y"}"#
        );
        assert_eq!(
            json_strip_nulls(r#"{"a": null, "b": [1, null]}"#, true, true).unwrap(),
            r#"{"b": [1]}"#
        );
        assert_eq!(json_typeof(" 1.0 ").unwrap(), "number");
        let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(json_strip_nulls(&deep, true, false).unwrap(), deep);
    }
}
