//! The kernels of the built-in functions of a PostgreSQL session, by the C function in `prosrc`
//! of `pg_proc`.
//!
//! The binder resolves a call over `pg_proc` as PostgreSQL does. When the function that it finds
//! has a kernel here, it calls [`PREFIX`] and the `prosrc` of the function, with each argument
//! cast to its declared type. So one kernel serves every name of a function, such as `ceil` and
//! `ceiling`, and the types of the arguments are always the declared ones.

use rudb_common::{Result, Value};

use crate::pgmath;

/// The prefix of the name of a kernel of a C function.
pub const PREFIX: &str = "__rudb_pgproc_";

/// Whether the C function `src` has a kernel.
pub fn has(src: &str) -> bool {
    pgmath::SOURCES.binary_search(&src).is_ok()
}

/// The value of a call of a kernel of a C function, or `None` for any other name. A null
/// argument gives a null, as every function here is strict.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let Some(src) = name.strip_prefix(PREFIX) else { return Ok(None) };
    if args.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    pgmath::call(src, args)
}

/// The function of one `float8` of the kernel `name`, for the loop over a column.
pub(crate) fn float_unary(name: &str) -> Option<fn(f64) -> Result<f64>> {
    pgmath::float_unary(name.strip_prefix(PREFIX)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sources_are_sorted() {
        assert!(pgmath::SOURCES.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
