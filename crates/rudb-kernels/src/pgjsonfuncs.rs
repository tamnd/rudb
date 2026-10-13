//! The kernels of the functions of `pg_proc` that read a `json` or a `jsonb` value, over the port
//! of `jsonfuncs.c` in `rudb_pgtypes`: the functions of `->`, `->>`, `#>` and `#>>`,
//! `json_array_length`, `json_typeof`, `json_strip_nulls` and the `jsonb` form of each.
//!
//! The operators of the engine with these names are the ones of the pin, which read a path of
//! JSONPath and give another text for a part of a `json` value. So each operator of PostgreSQL
//! over `json` or `jsonb` is the kernel of its function, as [`OPERATORS`] has them.

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
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_object_field",
    "jsonb_object_field_text",
    "jsonb_strip_nulls",
    "jsonb_typeof",
];

/// The C functions of the operators `->`, `->>`, `#>` and `#>>`.
pub(crate) const OPERATORS: &[&str] = &[
    "json_array_element",
    "json_array_element_text",
    "json_extract_path",
    "json_extract_path_text",
    "json_object_field",
    "json_object_field_text",
    "jsonb_array_element",
    "jsonb_array_element_text",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_object_field",
    "jsonb_object_field_text",
];

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let base = src.strip_prefix("jsonb").or_else(|| src.strip_prefix("json")).unwrap_or(src);
    let as_text = base.ends_with("_text");
    let found = match (base.trim_end_matches("_text"), args) {
        ("_object_field", [Value::Varchar(doc), Value::Varchar(key)]) => {
            rudb_pgtypes::json_object_field(doc, key, as_text).map_err(placed)?
        }
        ("_array_element", [Value::Varchar(doc), Value::Integer(index)]) => {
            rudb_pgtypes::json_array_element(doc, *index, as_text).map_err(placed)?
        }
        ("_extract_path", [Value::Varchar(doc), Value::List { values, .. }]) => {
            let path = values
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(None),
                    Value::Varchar(step) => Ok(Some(step.as_str())),
                    other => Err(Error::internal(format!("a step of a path as {other:?}"))),
                })
                .collect::<Result<Vec<_>>>()?;
            rudb_pgtypes::json_extract_path(doc, &path, as_text).map_err(placed)?
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
}
