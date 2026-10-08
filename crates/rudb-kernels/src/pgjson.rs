//! The set functions of the PostgreSQL `json` and `jsonb` types: `json_each`, `json_each_text`,
//! `json_array_elements`, `json_array_elements_text` and `json_object_keys`, and the `jsonb` form
//! of each.
//!
//! The binder makes each call an `unnest` of the list that `__rudb_pg_json_set` gives for the
//! document, so the same call is a set of rows in a select list and in `FROM`. The kernel reads
//! the text of the document and not a parsed tree, because a `json` value keeps the text it was
//! written with, spaces and repeated keys too, and each value it gives is a piece of that text. A
//! `jsonb` value is in its normal form, which has the keys sorted and once each, so the same walk
//! gives what PostgreSQL gives for it.

use rudb_common::{Error, Field, LogicalType, Result, SqlState, Value};

/// The name the plan records for the call.
pub const KERNEL: &str = "__rudb_pg_json_set";

/// What a set function gives for each member of the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gives {
    /// The key and the value of each member of an object.
    Each,
    /// The key and the value of each member of an object, with the value as text.
    EachText,
    /// Each element of an array.
    Elements,
    /// Each element of an array, as text.
    ElementsText,
    /// The key of each member of an object.
    Keys,
}

/// One of the set functions, for `json` or for `jsonb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonSet {
    /// What the function gives.
    pub gives: Gives,
    /// Whether it is the `jsonb` form.
    pub binary: bool,
}

/// The functions, with the name of each.
const FUNCTIONS: [(&str, Gives, bool); 10] = [
    ("json_each", Gives::Each, false),
    ("json_each_text", Gives::EachText, false),
    ("json_array_elements", Gives::Elements, false),
    ("json_array_elements_text", Gives::ElementsText, false),
    ("json_object_keys", Gives::Keys, false),
    ("jsonb_each", Gives::Each, true),
    ("jsonb_each_text", Gives::EachText, true),
    ("jsonb_array_elements", Gives::Elements, true),
    ("jsonb_array_elements_text", Gives::ElementsText, true),
    ("jsonb_object_keys", Gives::Keys, true),
];

impl JsonSet {
    /// The function of this name, found without case, or `None` for any other name.
    #[must_use]
    pub fn of(name: &str) -> Option<Self> {
        let found = FUNCTIONS.iter().find(|(held, ..)| held.eq_ignore_ascii_case(name));
        found.map(|&(_, gives, binary)| Self { gives, binary })
    }

    /// The name of the function.
    #[must_use]
    pub fn name(self) -> &'static str {
        let found = FUNCTIONS
            .iter()
            .find(|&&(_, gives, binary)| (gives, binary) == (self.gives, self.binary));
        found.map_or("", |(name, ..)| name)
    }

    /// The type of the document the function reads.
    #[must_use]
    pub fn document(self) -> LogicalType {
        if self.binary { LogicalType::Jsonb } else { LogicalType::Json }
    }

    /// The type of each element of the list the kernel gives.
    #[must_use]
    pub fn element(self) -> LogicalType {
        let pair = |value: LogicalType| {
            LogicalType::Struct(vec![
                Field::new("key", LogicalType::Varchar),
                Field::new("value", value),
            ])
        };
        match self.gives {
            Gives::Each => pair(self.document()),
            Gives::EachText => pair(LogicalType::Varchar),
            Gives::Elements => self.document(),
            Gives::ElementsText | Gives::Keys => LogicalType::Varchar,
        }
    }

    /// The name of the one column of the function in `FROM`, when PostgreSQL gives it one whatever
    /// the alias is. The elements are an output parameter called `value`, and the keys are the
    /// result of the function, which takes the alias.
    #[must_use]
    pub fn column(self) -> Option<&'static str> {
        matches!(self.gives, Gives::Elements | Gives::ElementsText).then_some("value")
    }

    /// The error for a document that is not what the function takes apart.
    fn refused(self, kind: Kind) -> Error {
        let name = self.name();
        let message = match (self.gives, self.binary, kind) {
            (Gives::Each | Gives::EachText, false, Kind::Array) => {
                "cannot deconstruct an array as an object".to_owned()
            }
            (Gives::Each | Gives::EachText, false, _) => "cannot deconstruct a scalar".to_owned(),
            (Gives::Each | Gives::EachText, true, _) => {
                format!("cannot call {name} on a non-object")
            }
            (Gives::Elements | Gives::ElementsText, false, Kind::Object) => {
                format!("cannot call {name} on a non-array")
            }
            (Gives::Elements | Gives::ElementsText, false, _) => {
                format!("cannot call {name} on a scalar")
            }
            (Gives::Elements | Gives::ElementsText, true, Kind::Object) => {
                "cannot extract elements from an object".to_owned()
            }
            (Gives::Elements | Gives::ElementsText, true, _) => {
                "cannot extract elements from a scalar".to_owned()
            }
            (Gives::Keys, _, Kind::Array) => format!("cannot call {name} on an array"),
            (Gives::Keys, _, _) => format!("cannot call {name} on a scalar"),
        };
        Error::invalid_input(message).state(SqlState::INVALID_PARAMETER_VALUE).unplaced()
    }

    /// The list for one document.
    fn list(self, text: &str) -> Result<Value> {
        let bytes = text.as_bytes();
        let start = skip_space(bytes, 0);
        let kind = match bytes.get(start) {
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            _ => Kind::Scalar,
        };
        let wanted = match self.gives {
            Gives::Elements | Gives::ElementsText => Kind::Array,
            Gives::Each | Gives::EachText | Gives::Keys => Kind::Object,
        };
        if kind != wanted {
            return Err(self.refused(kind));
        }
        let mut values = Vec::new();
        for (key, value) in members(text, start, kind == Kind::Object) {
            let value_text = || match value.as_bytes().first() {
                Some(b'"') => Value::Varchar(unescape(value)),
                _ if value == "null" => Value::Null,
                _ => Value::Varchar(value.to_owned()),
            };
            let key = || Value::Varchar(unescape(key));
            values.push(match self.gives {
                Gives::Each => Value::Struct(vec![
                    ("key".to_owned(), key()),
                    ("value".to_owned(), Value::Varchar(value.to_owned())),
                ]),
                Gives::EachText => Value::Struct(vec![
                    ("key".to_owned(), key()),
                    ("value".to_owned(), value_text()),
                ]),
                Gives::Elements => Value::Varchar(value.to_owned()),
                Gives::ElementsText => value_text(),
                Gives::Keys => key(),
            });
        }
        Ok(Value::List { element: self.element(), values })
    }
}

/// What a document is at its top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    Scalar,
}

/// `__rudb_pg_json_set(document, function)`, or `None` for another function.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    if name != KERNEL {
        return Ok(None);
    }
    let [document, function] = args else {
        return Err(Error::internal(format!("{KERNEL} takes 2 arguments")));
    };
    let Value::Varchar(function) = function else {
        return Err(Error::internal(format!("{KERNEL} needs the name of the function")));
    };
    let set = JsonSet::of(function)
        .ok_or_else(|| Error::internal(format!("{function} is not a json set function")))?;
    match document {
        // Every one of the functions is strict.
        Value::Varchar(text) => set.list(text).map(Some),
        _ => Ok(Some(Value::Null)),
    }
}

/// The first byte at or after `at` that is not a space of JSON.
fn skip_space(bytes: &[u8], mut at: usize) -> usize {
    while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        at += 1;
    }
    at
}

/// The byte just after the value that starts at `at`.
fn value_end(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at) {
        Some(b'"') => string_end(bytes, at),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut here = at;
            while let Some(&byte) = bytes.get(here) {
                match byte {
                    b'"' => {
                        here = string_end(bytes, here);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return here + 1;
                        }
                    }
                    _ => {}
                }
                here += 1;
            }
            here
        }
        _ => {
            let mut here = at;
            while let Some(byte) = bytes.get(here) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    break;
                }
                here += 1;
            }
            here
        }
    }
}

/// The byte just after the string that starts with the quote at `at`.
fn string_end(bytes: &[u8], at: usize) -> usize {
    let mut here = at + 1;
    while let Some(&byte) = bytes.get(here) {
        match byte {
            b'\\' => here += 2,
            b'"' => return here + 1,
            _ => here += 1,
        }
    }
    here
}

/// The members of the object or the array that starts at `start`, each as the text of its key and
/// the text of its value. An element of an array has an empty key.
fn members(text: &str, start: usize, object: bool) -> Vec<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = skip_space(bytes, start + 1);
    while let Some(&byte) = bytes.get(at) {
        if matches!(byte, b'}' | b']') {
            break;
        }
        let mut key = "";
        if object {
            let end = string_end(bytes, at);
            key = &text[at..end];
            at = skip_space(bytes, end);
            // The colon.
            at = skip_space(bytes, at + 1);
        }
        let end = value_end(bytes, at);
        out.push((key, &text[at..end]));
        at = skip_space(bytes, end);
        if bytes.get(at) == Some(&b',') {
            at = skip_space(bytes, at + 1);
        }
    }
    out
}

/// The text a JSON string stands for, given with its quotes.
fn unescape(quoted: &str) -> String {
    let inner = quoted.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(quoted);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let unit = hex4(&mut chars);
                let code = match unit {
                    Some(high @ 0xD800..=0xDBFF) => {
                        let mut ahead = chars.clone();
                        let low = match (ahead.next(), ahead.next()) {
                            (Some('\\'), Some('u')) => hex4(&mut ahead),
                            _ => None,
                        };
                        match low {
                            Some(low @ 0xDC00..=0xDFFF) => {
                                chars = ahead;
                                Some(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
                            }
                            _ => None,
                        }
                    }
                    other => other,
                };
                out.push(code.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Four hex digits as a number.
fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let mut code = 0;
    for _ in 0..4 {
        code = code * 16 + chars.next()?.to_digit(16)?;
    }
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(function: &str, document: &str) -> Result<Vec<Value>> {
        let args = [Value::Varchar(document.to_owned()), Value::Varchar(function.to_owned())];
        match call(KERNEL, &args)? {
            Some(Value::List { values, .. }) => Ok(values),
            other => panic!("{other:?}"),
        }
    }

    fn texts(values: &[Value]) -> Vec<String> {
        values.iter().map(|value| format!("{value:?}")).collect()
    }

    #[test]
    fn a_json_document_gives_the_text_it_was_written_with() {
        let each = run("json_each", r#"{"a" : {"x" :  1}, "a": "y", "b":null}"#).unwrap();
        let pair = |key: &str, value: &str| {
            Value::Struct(vec![
                ("key".to_owned(), Value::Varchar(key.to_owned())),
                ("value".to_owned(), Value::Varchar(value.to_owned())),
            ])
        };
        assert_eq!(each, [pair("a", r#"{"x" :  1}"#), pair("a", r#""y""#), pair("b", "null")]);
        let text = run("json_each_text", r#"{"a":"q\"é", "b":null, "c":[1]}"#).unwrap();
        assert_eq!(
            texts(&text),
            texts(&[
                pair("a", "q\"é"),
                Value::Struct(vec![
                    ("key".to_owned(), Value::Varchar("b".to_owned())),
                    ("value".to_owned(), Value::Null),
                ]),
                pair("c", "[1]"),
            ])
        );
    }

    #[test]
    fn the_elements_and_the_keys_are_in_document_order() {
        let elements = run("json_array_elements", r#" [1, "a", null, [2, 3], {"b": 4}] "#).unwrap();
        let strings = |items: &[&str]| -> Vec<Value> {
            items.iter().map(|item| Value::Varchar((*item).to_owned())).collect()
        };
        assert_eq!(elements, strings(&["1", r#""a""#, "null", "[2, 3]", r#"{"b": 4}"#]));
        let text = run("json_array_elements_text", r#"["a", null, 2]"#).unwrap();
        assert_eq!(
            text,
            [Value::Varchar("a".to_owned()), Value::Null, Value::Varchar("2".to_owned())]
        );
        assert_eq!(
            run("json_object_keys", r#"{"b":1,"a":2,"b":3}"#).unwrap(),
            strings(&["b", "a", "b"])
        );
        assert_eq!(run("json_array_elements", "[]").unwrap(), []);
        assert_eq!(run("json_each", "{ }").unwrap(), []);
        let emoji = run("json_array_elements_text", r#"["😀"]"#).unwrap();
        assert_eq!(emoji, strings(&["😀"]));
    }

    #[test]
    fn a_document_of_the_wrong_kind_is_refused_as_postgresql_refuses_it() {
        let refused = |function: &str, document: &str| {
            let error = run(function, document).unwrap_err();
            assert_eq!(error.sqlstate(), Some(SqlState::INVALID_PARAMETER_VALUE));
            error.message().to_owned()
        };
        assert_eq!(refused("json_each", "[1]"), "cannot deconstruct an array as an object");
        assert_eq!(refused("json_each_text", "1"), "cannot deconstruct a scalar");
        assert_eq!(refused("jsonb_each", "[1]"), "cannot call jsonb_each on a non-object");
        assert_eq!(
            refused("json_array_elements", r#"{"a":1}"#),
            "cannot call json_array_elements on a non-array"
        );
        assert_eq!(
            refused("json_array_elements_text", "null"),
            "cannot call json_array_elements_text on a scalar"
        );
        assert_eq!(
            refused("jsonb_array_elements", r#"{"a":1}"#),
            "cannot extract elements from an object"
        );
        assert_eq!(
            refused("jsonb_array_elements_text", "2"),
            "cannot extract elements from a scalar"
        );
        assert_eq!(refused("json_object_keys", "[1]"), "cannot call json_object_keys on an array");
        assert_eq!(
            refused("jsonb_object_keys", r#""s""#),
            "cannot call jsonb_object_keys on a scalar"
        );
    }

    #[test]
    fn a_null_document_is_a_null_list() {
        let args = [Value::Null, Value::Varchar("json_each".to_owned())];
        assert_eq!(call(KERNEL, &args).unwrap(), Some(Value::Null));
    }
}
