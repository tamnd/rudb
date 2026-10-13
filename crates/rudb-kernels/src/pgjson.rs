//! The set functions of the PostgreSQL `json` and `jsonb` types: `json_each`, `json_each_text`,
//! `json_array_elements`, `json_array_elements_text` and `json_object_keys`, and the `jsonb` form
//! of each.
//!
//! The binder makes each call an `unnest` of the list that `__rudb_pg_json_set` gives for the
//! document, so the same call is a set of rows in a select list and in `FROM`. The reader of
//! `rudb_pgtypes::json_each` and the others keeps the place of each value in the text, because a
//! `json` value keeps the text it was written with, spaces and repeated keys too, and each value
//! it gives is a piece of that text. A `jsonb` value is in its normal form, which has the keys
//! sorted and once each, so the same reader gives what PostgreSQL gives for it.

use rudb_common::{Error, Field, LogicalType, Result, Value};

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

    /// The list for one document, from the reader of `rudb_pgtypes`, which gives the errors of
    /// PostgreSQL for a document that is not what the function takes apart.
    fn list(self, text: &str) -> Result<Value> {
        let name = self.name();
        let as_text = matches!(self.gives, Gives::EachText | Gives::ElementsText);
        let failed = |error: rudb_pgtypes::TypeError| Error::from(error).unplaced();
        let text_of = |value: Option<String>| value.map_or(Value::Null, Value::Varchar);
        let values = match self.gives {
            Gives::Each | Gives::EachText => {
                rudb_pgtypes::json_each(text, name, as_text, self.binary)
                    .map_err(failed)?
                    .into_iter()
                    .map(|(key, value)| {
                        Value::Struct(vec![
                            ("key".to_owned(), Value::Varchar(key)),
                            ("value".to_owned(), text_of(value)),
                        ])
                    })
                    .collect()
            }
            Gives::Elements | Gives::ElementsText => {
                rudb_pgtypes::json_array_elements(text, name, as_text, self.binary)
                    .map_err(failed)?
                    .into_iter()
                    .map(text_of)
                    .collect()
            }
            Gives::Keys => rudb_pgtypes::json_object_keys(text, name)
                .map_err(failed)?
                .into_iter()
                .map(Value::Varchar)
                .collect(),
        };
        Ok(Value::List { element: self.element(), values })
    }
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

#[cfg(test)]
mod tests {
    use rudb_common::SqlState;

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
