//! `format_type` of `pg_proc`, as `format_type.c` writes it, over the port in `rudb_pgtypes`.
//!
//! The function is not strict. A null OID gives a null, and a null modifier is no modifier, which
//! is not the same as a modifier of -1: `format_type(1042, NULL)` is `character` and
//! `format_type(1042, -1)` is `bpchar`.

use rudb_common::{Result, Value};
use rudb_pgtypes::format_type_extended;

/// The C functions of this module, sorted.
pub(crate) const SOURCES: &[&str] = &["format_type"];

/// The C functions of this module that are not strict: they see a null argument.
pub(crate) const NULLS: &[&str] = &["format_type"];

/// The value of the C function `src` over `args`, or `None` for another function.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    let value = match (src, args) {
        ("format_type", [Value::Null, _]) => Value::Null,
        ("format_type", [Value::UInteger(oid), Value::Null]) => {
            Value::Varchar(format_type_extended(*oid, -1, false)?)
        }
        ("format_type", [Value::UInteger(oid), Value::Integer(typmod)]) => {
            Value::Varchar(format_type_extended(*oid, *typmod, true)?)
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_modifier_is_no_modifier_and_a_null_type_is_null() {
        let format = |args: &[Value]| call("format_type", args).unwrap().unwrap();
        assert_eq!(
            format(&[Value::UInteger(1042), Value::Null]),
            Value::Varchar("character".into())
        );
        assert_eq!(
            format(&[Value::UInteger(1042), Value::Integer(-1)]),
            Value::Varchar("bpchar".into())
        );
        assert_eq!(format(&[Value::Null, Value::Integer(5)]), Value::Null);
        assert!(call("format_type", &[Value::UInteger(1186), Value::Integer(0)]).is_err());
    }
}
