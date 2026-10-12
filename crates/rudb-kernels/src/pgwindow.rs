//! The check that a window function of PostgreSQL makes when it computes its first row, as
//! `WinCheckAndInitializeNullTreatment` in `nodeWindowAgg.c` makes it.
//!
//! Only `lag`, `lead`, `first_value`, `last_value` and `nth_value` take `RESPECT NULLS` or
//! `IGNORE NULLS`. The other window functions refuse it on their first row, so the error comes
//! after the row description, it has no position, and a query over no rows does not get it. The
//! binder puts the value of such a call through the kernel here, with the name of the function,
//! and the kernel refuses each row that it sees.

use rudb_common::{Error, Result, SqlState, Value};

/// The kernels of this module, sorted. They are not C functions of PostgreSQL.
pub(crate) const SOURCES: &[&str] = &["window_null_treatment"];

/// The kernels that see a null argument, because a call such as `ntile(null)` is still refused.
pub(crate) const NULLS: &[&str] = &["window_null_treatment"];

/// The value of the kernel `src` over `args`, or `None` for another kernel.
pub(crate) fn call(src: &str, args: &[Value]) -> Result<Option<Value>> {
    match (src, args) {
        ("window_null_treatment", [_, Value::Varchar(name)]) => Err(Error::not_implemented(
            format!("function {name} does not allow RESPECT/IGNORE NULLS"),
        )
        .state(SqlState::FEATURE_NOT_SUPPORTED)
        .unplaced()),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_row_is_refused_with_the_name_of_the_function() {
        let args = [Value::Null, Value::Varchar("ntile".to_string())];
        let error = call("window_null_treatment", &args).unwrap_err();
        assert_eq!(error.message(), "function ntile does not allow RESPECT/IGNORE NULLS");
        assert!(call("window_other", &args).unwrap().is_none());
    }
}
