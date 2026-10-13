//! The kernels of the functions of `pg_proc` over a `json` or a `jsonb` value, over the port of
//! `jsonfuncs.c` in `rudb_pgtypes`.
//!
//! The functions that read a value are those of `->`, `->>`, `#>` and `#>>`,
//! `json_array_length`, `json_typeof`, `json_strip_nulls` and the `jsonb` form of each. The
//! functions of `jsonb` that test a value or give a changed value are those of `?`, `?|`, `?&`,
//! `@>`, `<@`, `||`, `-` and `#-`, `jsonb_set`, `jsonb_set_lax`, `jsonb_insert` and
//! `jsonb_pretty`.
//!
//! The operators of the engine with these names are the ones of the pin, which read a path of
//! JSONPath, test a list or join two texts. So each operator of PostgreSQL over `json` or `jsonb`
//! is the kernel of its function, as [`OPERATORS`] has them.

use rudb_common::{Error, Result, Value};
use rudb_pgtypes::TypeError;

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "json_array_element",
    "json_array_element_text",
    "json_array_length",
    "json_extract_path",
    "json_extract_path_text",
    "json_object_field",
    "json_object_field_text",
    "json_strip_nulls",
    "json_typeof",
    "jsonb_array_element",
    "jsonb_array_element_text",
    "jsonb_array_length",
    "jsonb_concat",
    "jsonb_contained",
    "jsonb_contains",
    "jsonb_delete",
    "jsonb_delete_array",
    "jsonb_delete_idx",
    "jsonb_delete_path",
    "jsonb_exists",
    "jsonb_exists_all",
    "jsonb_exists_any",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_insert",
    "jsonb_object_field",
    "jsonb_object_field_text",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_set_lax",
    "jsonb_strip_nulls",
    "jsonb_typeof",
];

/// The C functions of this module that are not strict, as `proisstrict` has them.
pub(crate) const NULLS: &[&str] = &["jsonb_set_lax"];

/// The C functions of the operators `->`, `->>`, `#>`, `#>>`, `?`, `?|`, `?&`, `@>`, `<@`, `||`,
/// `-` and `#-`.
pub(crate) const OPERATORS: &[&str] = &[
    "json_array_element",
    "json_array_element_text",
    "json_extract_path",
    "json_extract_path_text",
    "json_object_field",
    "json_object_field_text",
    "jsonb_array_element",
    "jsonb_array_element_text",
    "jsonb_concat",
    "jsonb_contained",
    "jsonb_contains",
    "jsonb_delete",
    "jsonb_delete_array",
    "jsonb_delete_idx",
    "jsonb_delete_path",
    "jsonb_exists",
    "jsonb_exists_all",
    "jsonb_exists_any",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_object_field",
    "jsonb_object_field_text",
];

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    if let Some(value) = jsonb_call(src, args)? {
        return Ok(Some(value));
    }
    let base = src.strip_prefix("jsonb").or_else(|| src.strip_prefix("json")).unwrap_or(src);
    let as_text = base.ends_with("_text");
    let found = match (base.trim_end_matches("_text"), args) {
        ("_object_field", [Value::Varchar(doc), Value::Varchar(key)]) => {
            rudb_pgtypes::json_object_field(doc, key, as_text).map_err(placed)?
        }
        ("_array_element", [Value::Varchar(doc), Value::Integer(index)]) => {
            rudb_pgtypes::json_array_element(doc, *index, as_text).map_err(placed)?
        }
        ("_extract_path", [Value::Varchar(doc), path]) => {
            rudb_pgtypes::json_extract_path(doc, &texts(path)?, as_text).map_err(placed)?
        }
        ("_array_length", [Value::Varchar(doc)]) => {
            let length = rudb_pgtypes::json_array_length(doc).map_err(placed)?;
            return Ok(Some(Value::Integer(length)));
        }
        ("_typeof", [Value::Varchar(doc)]) => {
            Some(rudb_pgtypes::json_typeof(doc).map_err(placed)?.to_owned())
        }
        ("_strip_nulls", [Value::Varchar(doc), Value::Boolean(in_arrays)]) => {
            let jsonb = src.starts_with("jsonb");
            Some(rudb_pgtypes::json_strip_nulls(doc, *in_arrays, jsonb).map_err(placed)?)
        }
        _ => return Ok(None),
    };
    Ok(Some(found.map_or(Value::Null, Value::Varchar)))
}

/// The value of a C function over `jsonb` that tests a value or gives a changed one, or `None` for
/// another function.
fn jsonb_call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    use rudb_pgtypes as pg;
    let tested = match (src, args) {
        ("jsonb_exists", [Value::Varchar(doc), Value::Varchar(key)]) => pg::jsonb_exists(doc, key),
        ("jsonb_exists_any" | "jsonb_exists_all", [Value::Varchar(doc), keys]) => {
            pg::jsonb_exists_keys(doc, &texts(keys)?, src == "jsonb_exists_any")
        }
        ("jsonb_contains", [Value::Varchar(doc), Value::Varchar(other)]) => {
            pg::jsonb_contains(doc, other)
        }
        ("jsonb_contained", [Value::Varchar(doc), Value::Varchar(other)]) => {
            pg::jsonb_contains(other, doc)
        }
        _ => {
            let changed = match (src, args) {
                ("jsonb_concat", [Value::Varchar(doc), Value::Varchar(other)]) => {
                    pg::jsonb_concat(doc, other)
                }
                ("jsonb_delete", [Value::Varchar(doc), Value::Varchar(key)]) => {
                    pg::jsonb_delete(doc, &[Some(key)])
                }
                ("jsonb_delete_array", [Value::Varchar(doc), keys]) => {
                    pg::jsonb_delete(doc, &texts(keys)?)
                }
                ("jsonb_delete_idx", [Value::Varchar(doc), Value::Integer(index)]) => {
                    pg::jsonb_delete_index(doc, *index)
                }
                ("jsonb_delete_path", [Value::Varchar(doc), path]) => {
                    pg::jsonb_delete_path(doc, &texts(path)?)
                }
                (
                    "jsonb_set",
                    [Value::Varchar(doc), path, Value::Varchar(value), Value::Boolean(create)],
                ) => pg::jsonb_set(doc, &texts(path)?, value, *create),
                (
                    "jsonb_insert",
                    [Value::Varchar(doc), path, Value::Varchar(value), Value::Boolean(after)],
                ) => pg::jsonb_insert(doc, &texts(path)?, value, *after),
                ("jsonb_set_lax", [doc, path, value, create, treatment]) => {
                    let path = match path {
                        Value::Null => None,
                        path => Some(texts(path)?),
                    };
                    let changed = pg::jsonb_set_lax(
                        text(doc)?,
                        path.as_deref(),
                        text(value)?,
                        match create {
                            Value::Boolean(create) => Some(*create),
                            _ => None,
                        },
                        text(treatment)?,
                    );
                    return Ok(Some(changed.map_err(placed)?.map_or(Value::Null, Value::Varchar)));
                }
                ("jsonb_pretty", [Value::Varchar(doc)]) => pg::jsonb_pretty(doc),
                _ => return Ok(None),
            };
            return Ok(Some(Value::Varchar(changed.map_err(placed)?)));
        }
    };
    Ok(Some(Value::Boolean(tested.map_err(placed)?)))
}

/// The elements of a `text[]`, with `None` for a null.
fn texts(list: &Value) -> Result<Vec<Option<&str>>> {
    let Value::List { values, .. } = list else {
        return Err(Error::internal(format!("a text[] as {list:?}")));
    };
    values.iter().map(text).collect()
}

/// A `text` or a `jsonb` value, with `None` for a null.
fn text(value: &Value) -> Result<Option<&str>> {
    match value {
        Value::Null => Ok(None),
        Value::Varchar(text) => Ok(Some(text)),
        other => Err(Error::internal(format!("a text as {other:?}"))),
    }
}

/// An error of the port, which has no place in the statement.
fn placed(error: TypeError) -> Error {
    Error::from(error).unplaced()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Value {
        Value::Varchar(text.to_owned())
    }

    #[test]
    fn the_operators_read_a_part_of_the_value() {
        let doc = text(r#"{"a": [1, {"b" : "x"}]}"#);
        let got = call("json_object_field", &[doc.clone(), text("a")]).unwrap();
        assert_eq!(got, Some(text(r#"[1, {"b" : "x"}]"#)));
        let path = Value::List {
            element: rudb_common::LogicalType::Varchar,
            values: vec![text("a"), text("-1"), text("b")],
        };
        assert_eq!(call("json_extract_path_text", &[doc.clone(), path]).unwrap(), Some(text("x")));
        let got = call("jsonb_array_element", &[doc.clone(), Value::Integer(0)]).unwrap();
        assert_eq!(got, Some(Value::Null));
        assert_eq!(call("json_typeof", &[doc]).unwrap(), Some(text("object")));
        let error = call("jsonb_array_length", &[text("1")]).unwrap_err();
        assert_eq!(error.message(), "cannot get array length of a scalar");
    }

    #[test]
    fn the_jsonb_operators_test_and_change_a_value() {
        let doc = text(r#"{"a": 1, "b": [1, 2]}"#);
        let keys = Value::List {
            element: rudb_common::LogicalType::Varchar,
            values: vec![Value::Null, text("b")],
        };
        let got = call("jsonb_exists_all", &[doc.clone(), keys.clone()]).unwrap();
        assert_eq!(got, Some(Value::Boolean(true)));
        let got = call("jsonb_contained", &[text(r#"{"b": [2]}"#), doc.clone()]).unwrap();
        assert_eq!(got, Some(Value::Boolean(true)));
        assert_eq!(
            call("jsonb_delete_array", &[doc.clone(), keys]).unwrap(),
            Some(text(r#"{"a": 1}"#))
        );
        let lax = [doc.clone(), Value::Null, Value::Null, Value::Boolean(true), text("x")];
        assert_eq!(call("jsonb_set_lax", &lax).unwrap(), Some(Value::Null));
        let error = call("jsonb_delete_idx", &[doc, Value::Integer(0)]).unwrap_err();
        assert_eq!(error.message(), "cannot delete from object using integer index");
    }
}
