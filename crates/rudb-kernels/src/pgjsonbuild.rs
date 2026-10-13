//! The kernels of the functions of `pg_proc` that build a `json` or a `jsonb` value, over the port
//! in `rudb_pgtypes`, and the kernels that write one value as its JSON text.
//!
//! PostgreSQL writes a value into a new `json` value by the category of its type, in
//! `datum_to_json_internal`. The binder does the same with the kernels here: a number is its output
//! text, a date and a timestamp are in the ISO 8601 form of [`TIME`], a `json` value is its text,
//! an array is [`ARRAY`] over its elements and a row is [`RECORD`] over its fields. Any other value
//! is the string of its output text. So the functions of `pg_proc` in [`DATUMS`] take the JSON
//! text of each argument that can be of any type, and only put the pieces together.

use rudb_common::{Error, Result, SessionTimeZone, Value};
use rudb_pgtypes::{
    DateFormat, DateOrder, DateStyle, TypeError, date_from_unix, date_out, escape_json,
    timestamp_from_unix, timestamp_out, timestamptz_out,
};

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &[
    "array_to_json",
    "array_to_json_pretty",
    "json_build_array",
    "json_build_array_noargs",
    "json_build_object",
    "json_build_object_noargs",
    "json_object",
    "json_object_two_arg",
    "jsonb_build_array",
    "jsonb_build_array_noargs",
    "jsonb_build_object",
    "jsonb_build_object_noargs",
    "jsonb_object",
    "jsonb_object_two_arg",
    "row_to_json",
    "row_to_json_pretty",
    "to_json",
    "to_jsonb",
];

/// The C functions of this module that are not strict: they see a null argument.
pub(crate) const NULLS: &[&str] = &[
    "json_build_array",
    "json_build_array_noargs",
    "json_build_object",
    "json_build_object_noargs",
    "jsonb_build_array",
    "jsonb_build_array_noargs",
    "jsonb_build_object",
    "jsonb_build_object_noargs",
];

/// The C functions of this module that take each argument of a polymorphic type, `record` or
/// `any` as its JSON text.
pub(crate) const DATUMS: &[&str] = &[
    "array_to_json",
    "array_to_json_pretty",
    "json_build_array",
    "json_build_object",
    "jsonb_build_array",
    "jsonb_build_object",
    "row_to_json",
    "row_to_json_pretty",
    "to_json",
    "to_jsonb",
];

/// The JSON text of a value that is not an array, a row or a date, from the value and the name of
/// its category: `bool` for a boolean, `number` for the output text of a number, `json` for the
/// text of a `json` or a `jsonb` value, and `string` for the output text of any other value.
pub const LEAF: &str = "__rudb_pg_json_leaf";

/// The JSON text of a `date`, a `timestamp` or a `timestamptz`, a string in the form of ISO 8601
/// that `to_json` writes in any `DateStyle`, with the zone of the session for a `timestamptz`.
pub const TIME: &str = "__rudb_pg_json_time";

/// The JSON text of an array, from the list of the JSON text of each element.
pub const ARRAY: &str = "__rudb_pg_json_array";

/// The JSON text of a row, from the row, the list of the names of its fields and the list of the
/// JSON text of each field. The row is there only so that a null row is a null.
pub const RECORD: &str = "__rudb_pg_json_record";

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let jsonb = src.starts_with("jsonb");
    let text = match (src, args) {
        ("to_json" | "array_to_json" | "row_to_json", [Value::Varchar(text)]) => text.clone(),
        ("to_jsonb", [Value::Varchar(text)]) => rudb_pgtypes::to_jsonb(text).map_err(placed)?,
        ("array_to_json_pretty" | "row_to_json_pretty", [Value::Varchar(text), pretty]) => {
            match pretty {
                Value::Boolean(true) => rudb_pgtypes::pretty_outer(text),
                _ => text.clone(),
            }
        }
        ("json_build_array" | "jsonb_build_array", [values]) => {
            rudb_pgtypes::build_array(&texts(values)?, jsonb).map_err(placed)?
        }
        ("json_build_object" | "jsonb_build_object", [values]) => {
            rudb_pgtypes::build_object(&texts(values)?, jsonb).map_err(placed)?
        }
        ("json_build_array_noargs" | "jsonb_build_array_noargs", []) => "[]".into(),
        ("json_build_object_noargs" | "jsonb_build_object_noargs", []) => "{}".into(),
        ("json_object" | "jsonb_object", [array]) => {
            let pairs = pairs_of_one(array)?;
            rudb_pgtypes::object_of_texts(&pairs, jsonb).map_err(placed)?
        }
        ("json_object_two_arg" | "jsonb_object_two_arg", [keys, values]) => {
            let pairs = pairs_of_two(keys, values)?;
            rudb_pgtypes::object_of_texts(&pairs, jsonb).map_err(placed)?
        }
        _ => return Ok(None),
    };
    Ok(Some(Value::Varchar(text)))
}

/// The value of one of the kernels that write one value as its JSON text, other than [`TIME`],
/// or `None` for another name.
pub(crate) fn writer(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let mut out = String::new();
    match (name, args) {
        (LEAF | ARRAY, [Value::Null, ..]) | (RECORD, [Value::Null, _, _]) => {
            return Ok(Some(Value::Null));
        }
        (LEAF, [value, Value::Varchar(category)]) => match (category.as_str(), value) {
            ("bool", Value::Boolean(flag)) => out.push_str(if *flag { "true" } else { "false" }),
            ("number", Value::Varchar(text)) if rudb_pgtypes::valid_json_number(text) => {
                out.push_str(text);
            }
            ("number" | "string", Value::Varchar(text)) => escape_json(text, &mut out),
            ("json", Value::Varchar(text)) => out.push_str(text),
            _ => return Err(Error::internal(format!("{LEAF} of {value:?} as {category}"))),
        },
        (ARRAY, [Value::List { values, .. }]) => {
            out.push('[');
            join(values.iter().map(element), ",", &mut out)?;
            out.push(']');
        }
        (RECORD, [_, Value::List { values: names, .. }, Value::List { values, .. }]) => {
            out.push('{');
            for (at, (name, value)) in names.iter().zip(values).enumerate() {
                if at > 0 {
                    out.push(',');
                }
                let Value::Varchar(name) = name else {
                    return Err(Error::internal(format!("{RECORD} with the name {name:?}")));
                };
                escape_json(name, &mut out);
                out.push(':');
                out.push_str(element(value)?);
            }
            out.push('}');
        }
        _ => return Ok(None),
    }
    Ok(Some(Value::Varchar(out)))
}

/// The value of [`TIME`] over `args` in the time zone `zone`.
pub(crate) fn time(args: &[Value], zone: SessionTimeZone) -> Result<Value> {
    let xsd = DateFormat { style: DateStyle::Xsd, order: DateOrder::Mdy };
    let mut written = Vec::new();
    match args {
        [Value::Null] => return Ok(Value::Null),
        [Value::Date(days)] => date_out(date_from_unix(*days).map_err(placed)?, xsd, &mut written),
        [Value::Timestamp(micros)] => {
            let ts = timestamp_from_unix(*micros).map_err(placed)?;
            timestamp_out(ts, xsd, &mut written).map_err(placed)?;
        }
        [Value::TimestampTz(micros)] => {
            let ts = timestamp_from_unix(*micros).map_err(placed)?;
            timestamptz_out(ts, xsd, &zone, &mut written).map_err(placed)?;
        }
        _ => return Err(Error::internal(format!("{TIME} over {args:?}"))),
    }
    let mut out = String::new();
    escape_json(&String::from_utf8_lossy(&written), &mut out);
    Ok(Value::Varchar(out))
}

/// The JSON text of an element of an array or a field of a row, which is `null` for a null.
fn element(value: &Value) -> Result<&str> {
    match value {
        Value::Null => Ok("null"),
        Value::Varchar(text) => Ok(text),
        other => Err(Error::internal(format!("the JSON text of an element as {other:?}"))),
    }
}

/// Each JSON text of `pieces` with `separator` between them.
fn join<'a>(
    pieces: impl Iterator<Item = Result<&'a str>>,
    separator: &str,
    out: &mut String,
) -> Result<()> {
    for (at, piece) in pieces.enumerate() {
        if at > 0 {
            out.push_str(separator);
        }
        out.push_str(piece?);
    }
    Ok(())
}

/// The elements of a list of text, with `None` for a null.
fn texts(list: &Value) -> Result<Vec<Option<&str>>> {
    let Value::List { values, .. } = list else {
        return Err(Error::internal(format!("a list of text as {list:?}")));
    };
    values
        .iter()
        .map(|value| match value {
            Value::Null => Ok(None),
            Value::Varchar(text) => Ok(Some(text.as_str())),
            other => Err(Error::internal(format!("an element of a list of text as {other:?}"))),
        })
        .collect()
}

/// The keys and the values of `json_object` of one array: a list of keys and values in turn, or a
/// list of pairs.
fn pairs_of_one(array: &Value) -> Result<Vec<(Option<&str>, Option<&str>)>> {
    let Value::List { values, .. } = array else {
        return Err(Error::internal(format!("json_object of {array:?}")));
    };
    let shape = |message: &str| placed(rudb_pgtypes::object_shape_error(message));
    if values.iter().any(|value| matches!(value, Value::List { .. })) {
        let mut pairs = Vec::with_capacity(values.len());
        for pair in values {
            let Value::List { values: pair, .. } = pair else {
                return Err(shape("wrong number of array subscripts"));
            };
            if pair.iter().any(|value| matches!(value, Value::List { .. })) {
                return Err(shape("wrong number of array subscripts"));
            }
            let [name, value] = pair.as_slice() else {
                return Err(shape("array must have two columns"));
            };
            pairs.push((text(name)?, text(value)?));
        }
        return Ok(pairs);
    }
    if values.len() % 2 != 0 {
        return Err(shape("array must have even number of elements"));
    }
    values.chunks(2).map(|pair| Ok((text(&pair[0])?, text(&pair[1])?))).collect()
}

/// The keys and the values of `json_object` of two arrays, a list of keys and a list of values of
/// one length.
fn pairs_of_two<'a>(
    keys: &'a Value,
    values: &'a Value,
) -> Result<Vec<(Option<&'a str>, Option<&'a str>)>> {
    let shape = |message: &str| placed(rudb_pgtypes::object_shape_error(message));
    let (Value::List { values: keys, .. }, Value::List { values, .. }) = (keys, values) else {
        return Err(Error::internal("json_object of two arrays that are not lists"));
    };
    let nested = |list: &[Value]| list.iter().any(|value| matches!(value, Value::List { .. }));
    if nested(keys) || nested(values) {
        return Err(shape("wrong number of array subscripts"));
    }
    if keys.len() != values.len() {
        return Err(shape("mismatched array dimensions"));
    }
    keys.iter().zip(values).map(|(name, value)| Ok((text(name)?, text(value)?))).collect()
}

/// An element of a `text[]`, with `None` for a null.
fn text(value: &Value) -> Result<Option<&str>> {
    match value {
        Value::Null => Ok(None),
        Value::Varchar(text) => Ok(Some(text)),
        other => Err(Error::internal(format!("an element of a text[] as {other:?}"))),
    }
}

/// An error of PostgreSQL that has no position, as the errors of a function that runs have.
fn placed(error: TypeError) -> Error {
    Error::from(error).unplaced()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: &[Value]) -> Value {
        Value::List { element: rudb_common::LogicalType::Varchar, values: values.to_vec() }
    }

    fn text(text: &str) -> Value {
        Value::Varchar(text.into())
    }

    #[test]
    fn a_leaf_is_written_by_its_category() {
        let leaf =
            |value: Value, category: &str| writer(LEAF, &[value, text(category)]).unwrap().unwrap();
        assert_eq!(leaf(Value::Boolean(true), "bool"), text("true"));
        assert_eq!(leaf(text("1.50"), "number"), text("1.50"));
        assert_eq!(leaf(text("NaN"), "number"), text("\"NaN\""));
        assert_eq!(leaf(text("a\"b"), "string"), text("\"a\\\"b\""));
        assert_eq!(leaf(text("{\"a\" : 1}"), "json"), text("{\"a\" : 1}"));
        assert_eq!(leaf(Value::Null, "number"), Value::Null);
    }

    #[test]
    fn an_array_and_a_row_write_a_null_element_as_null() {
        let array = writer(ARRAY, &[list(&[text("1"), Value::Null])]).unwrap().unwrap();
        assert_eq!(array, text("[1,null]"));
        let names = list(&[text("f1"), text("f2")]);
        let row = Value::Struct(vec![]);
        let record = writer(RECORD, &[row, names.clone(), list(&[text("1"), Value::Null])]);
        assert_eq!(record.unwrap().unwrap(), text("{\"f1\":1,\"f2\":null}"));
        let record = writer(RECORD, &[Value::Null, names, list(&[Value::Null, Value::Null])]);
        assert_eq!(record.unwrap().unwrap(), Value::Null);
    }

    #[test]
    fn a_time_is_in_the_form_of_iso_8601() {
        let zone = SessionTimeZone::default();
        assert_eq!(time(&[Value::Date(18262)], zone).unwrap(), text("\"2020-01-01\""));
        let stamp = Value::Timestamp(1_577_872_800_000_000);
        assert_eq!(time(&[stamp], zone).unwrap(), text("\"2020-01-01T10:00:00\""));
        let instant = Value::TimestampTz(1_577_872_800_000_000);
        assert_eq!(time(&[instant], zone).unwrap(), text("\"2020-01-01T10:00:00+00:00\""));
    }

    #[test]
    fn json_object_takes_the_shapes_that_postgresql_takes() {
        let object = |args: &[Value]| call("json_object", args).map(|value| value.unwrap());
        let flat = list(&[text("a"), text("1"), text("b"), Value::Null]);
        assert_eq!(object(&[flat]).unwrap(), text("{\"a\" : \"1\", \"b\" : null}"));
        let pairs = list(&[list(&[text("a"), text("1")])]);
        assert_eq!(object(&[pairs]).unwrap(), text("{\"a\" : \"1\"}"));
        let odd = object(&[list(&[text("a")])]).unwrap_err();
        assert_eq!(odd.message(), "array must have even number of elements");
        let wide = object(&[list(&[list(&[text("a"), text("1"), text("2")])])]).unwrap_err();
        assert_eq!(wide.message(), "array must have two columns");
        let two = call("json_object_two_arg", &[list(&[text("a")]), list(&[])]).unwrap_err();
        assert_eq!(two.message(), "mismatched array dimensions");
    }
}
