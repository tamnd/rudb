//! The assignment to a subscript of a `jsonb` value in a PostgreSQL session, `SET j['a'][0] = v`,
//! as `jsonb_subscript_assign` in `jsonbsubs.c` makes it.
//!
//! The binder writes the assignment as `__rudb_pg_jsonb_assign(value, path, new, array)`. The
//! path has the text of each subscript, and `array` says whether the first subscript is an
//! integer. A null value is an empty array when it is and an empty object when it is not, and a
//! null `new` is the JSON null.

use rudb_common::{Error, Result, SqlState, Value};

/// The name the plan records for the call.
pub const KERNEL: &str = "__rudb_pg_jsonb_assign";

/// `__rudb_pg_jsonb_assign(value, path, new, array)`, or `None` for another function.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    if name != KERNEL {
        return Ok(None);
    }
    let [value, Value::List { values: path, .. }, new, Value::Boolean(array)] = args else {
        return Err(Error::internal(format!("{KERNEL} of {args:?}")));
    };
    let mut steps = Vec::with_capacity(path.len());
    for step in path {
        match step {
            Value::Varchar(step) => steps.push(step.as_str()),
            Value::Null => {
                return Err(Error::invalid_input("jsonb subscript in assignment must not be null")
                    .state(SqlState::NULL_VALUE_NOT_ALLOWED)
                    .unplaced());
            }
            other => return Err(Error::internal(format!("{KERNEL} of a step {other:?}"))),
        }
    }
    let value = match value {
        Value::Varchar(value) => value.as_str(),
        _ if *array => "[]",
        _ => "{}",
    };
    let new = match new {
        Value::Varchar(new) => new.as_str(),
        _ => "null",
    };
    let changed = rudb_pgtypes::jsonb_set_element(value, &steps, new)
        .map_err(|error| Error::from(error).unplaced())?;
    Ok(Some(Value::Varchar(changed)))
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    fn assign(value: Value, path: &[Value], new: Value, array: bool) -> Result<Value> {
        let path = Value::List { element: LogicalType::Varchar, values: path.to_vec() };
        Ok(call(KERNEL, &[value, path, new, Value::Boolean(array)])?.unwrap())
    }

    fn text(text: &str) -> Value {
        Value::Varchar(text.to_owned())
    }

    #[test]
    fn a_null_value_is_the_container_that_the_first_subscript_asks_for() {
        assert_eq!(assign(Value::Null, &[text("0")], text("1"), true).unwrap(), text("[1]"));
        assert_eq!(
            assign(Value::Null, &[text("0")], text("1"), false).unwrap(),
            text(r#"{"0": 1}"#)
        );
        assert_eq!(
            assign(text("{}"), &[text("a")], Value::Null, false).unwrap(),
            text(r#"{"a": null}"#)
        );
        let error = assign(text("{}"), &[Value::Null], text("1"), false).unwrap_err();
        assert_eq!(error.sqlstate(), Some(SqlState::NULL_VALUE_NOT_ALLOWED));
    }
}
