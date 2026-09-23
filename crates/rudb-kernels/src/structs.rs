//! Building a struct and picking a field out of one, over whole vectors.
//!
//! A struct vector is one child vector per field beside a validity of its own, so building one out
//! of columns is putting them side by side and picking a field is handing back one of them with
//! the struct's nulls laid over it. Neither reads a row. The row path in [`crate::scalar`] gives
//! the same answers one value at a time, for the constant and dictionary forms these step aside
//! for.

use rudb_common::{Error, LogicalType, Result, Value};
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
        ("struct_extract", [input, key]) => Ok(Some(match (input, place(key)) {
            (Value::Struct(fields), Some(at)) => {
                fields.get(at).map_or(Value::Null, |(_, value)| value.clone())
            }
            _ => Value::Null,
        })),
        _ => Ok(None),
    }
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
