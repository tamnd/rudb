//! Building a struct and picking a field out of one, over whole vectors.
//!
//! A struct vector is one child vector per field beside a validity of its own, so building one out
//! of columns is putting them side by side and picking a field is handing back one of them with
//! the struct's nulls laid over it. Neither reads a row. The row path in [`crate::scalar`] gives
//! the same answers one value at a time, for the constant and dictionary forms these step aside
//! for.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_vector::{Form, Vector};

/// The one value path for both calls, or `None` for any other name.
pub(crate) fn value(name: &str, args: &[Value], returns: &LogicalType) -> Result<Option<Value>> {
    match (name, args) {
        ("struct_pack", _) => {
            let LogicalType::Struct(fields) = returns else {
                return Err(Error::internal(format!("struct_pack returning {returns}")));
            };
            let packed =
                fields.iter().zip(args).map(|(field, arg)| (field.name.clone(), arg.clone()));
            Ok(Some(Value::Struct(packed.collect())))
        }
        ("struct_insert" | "struct_update" | "struct_concat", _) => {
            let LogicalType::Struct(fields) = returns else {
                return Err(Error::internal(format!("{name} returning {returns}")));
            };
            Ok(Some(Value::Struct(merged(name, args, fields))))
        }
        ("struct_keys" | "struct_values", [Value::Null]) => Ok(Some(Value::Null)),
        ("struct_keys", [Value::Struct(fields)]) => Ok(Some(Value::List {
            element: LogicalType::Varchar,
            values: fields.iter().map(|(name, _)| Value::Varchar(name.clone())).collect(),
        })),
        ("struct_values", [Value::Struct(fields)]) => Ok(Some(Value::Struct(
            fields.iter().map(|(_, value)| (String::new(), value.clone())).collect(),
        ))),
        ("struct_contains" | "struct_position", [held, needle]) => {
            let Value::Struct(fields) = held else {
                return Ok(Some(Value::Null));
            };
            if needle.is_null() {
                return Ok(Some(Value::Null));
            }
            let at = fields.iter().position(|(_, value)| {
                !value.is_null()
                    && crate::compare::order(value, needle).ok() == Some(std::cmp::Ordering::Equal)
            });
            Ok(Some(match (name, at) {
                ("struct_contains", at) => Value::Boolean(at.is_some()),
                (_, Some(at)) => Value::Integer(at as i32 + 1),
                (_, None) => Value::Null,
            }))
        }
        ("struct_extract", [input, key]) => Ok(Some(match (input, place(key)) {
            (Value::Struct(fields), Some(at)) => {
                fields.get(at).map_or(Value::Null, |(_, value)| value.clone())
            }
            _ => Value::Null,
        })),
        _ => Ok(None),
    }
}

/// The fields of `struct_insert`, `struct_update` or `struct_concat`, laid out as the binder's type
/// says. A null struct argument gives nulls for its own fields and the rest still come through,
/// which is what the pin does with `struct_insert(NULL::STRUCT(a INT), b := 2)`.
///
/// An insert or a concatenation is the fields of each argument one after another. An update keeps
/// the first struct's places, so a field in the first place that the second struct names takes the
/// second one's value, and the second struct's other fields are the ones added at the end.
fn merged(name: &str, args: &[Value], fields: &[Field]) -> Vec<(String, Value)> {
    let parts: Vec<Vec<Value>> = args
        .iter()
        .map(|arg| match arg {
            Value::Struct(held) => held.iter().map(|(_, value)| value.clone()).collect(),
            _ => Vec::new(),
        })
        .collect();
    let mut values: Vec<Value> = Vec::with_capacity(fields.len());
    if name == "struct_update" {
        let [_, Value::Struct(added)] = args else {
            return fields.iter().map(|field| (field.name.clone(), Value::Null)).collect();
        };
        for (at, field) in fields.iter().enumerate() {
            let replaced = added.iter().find(|(name, _)| name.eq_ignore_ascii_case(&field.name));
            values.push(match replaced {
                Some((_, value)) => value.clone(),
                None => parts[0].get(at).cloned().unwrap_or(Value::Null),
            });
        }
    } else {
        // A null has no fields to count, so a null argument is as wide as what the others leave
        // over. With more than one null there is no telling where one ends, and every field is
        // left null.
        let known: usize = parts.iter().map(Vec::len).sum();
        let nulls = args.iter().filter(|arg| arg.is_null()).count();
        if nulls <= 1 {
            for (arg, part) in args.iter().zip(&parts) {
                if arg.is_null() {
                    values.resize(values.len() + fields.len().saturating_sub(known), Value::Null);
                } else {
                    values.extend(part.iter().cloned());
                }
            }
        }
    }
    values.resize(fields.len(), Value::Null);
    fields.iter().map(|field| field.name.clone()).zip(values).collect()
}

/// The field a `struct_extract` key names, which the binder records as its place counted from one.
fn place(key: &Value) -> Option<usize> {
    key.as_i64().and_then(|at| usize::try_from(at).ok()).and_then(|at| at.checked_sub(1))
}

/// The vector path for both calls, or `None` to leave the call to the row path.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
) -> Result<Option<Vector>> {
    match (name, args) {
        ("struct_pack", [_, ..]) => {
            let LogicalType::Struct(fields) = returns else {
                return Err(Error::internal(format!("struct_pack returning {returns}")));
            };
            let mut children = Vec::with_capacity(args.len());
            for (field, arg) in fields.iter().zip(args) {
                children.push((field.name.clone(), arg.as_ref().flatten()?));
            }
            Ok(Some(Vector::structure(children)?))
        }
        ("struct_extract", [input, key]) => {
            let input = input.as_ref();
            let (Some(children), LogicalType::Struct(fields)) =
                (input.struct_parts(), input.logical_type())
            else {
                return Ok(None);
            };
            let Some(at) = key.as_ref().try_value_at(0).ok().as_ref().and_then(place) else {
                return Ok(None);
            };
            if at >= fields.len() {
                return Ok(None);
            }
            let child = &children[at];
            let child =
                if child.form() == Form::Flat { (**child).clone() } else { child.flatten()? };
            let validity = input.validity().and(child.validity(), input.len());
            Ok(Some(child.with_validity(validity)))
        }
        _ => Ok(None),
    }
}
