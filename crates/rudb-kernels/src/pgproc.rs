//! The kernels of the built-in functions of a PostgreSQL session, by the C function in `prosrc`
//! of `pg_proc`.
//!
//! The binder resolves a call over `pg_proc` as PostgreSQL does. When the function that it finds
//! has a kernel here, it calls [`PREFIX`] and the `prosrc` of the function, with each argument
//! cast to its declared type. So one kernel serves every name of a function, such as `ceil` and
//! `ceiling`, and the types of the arguments are always the declared ones.

use rudb_common::{Result, Value};

use crate::{pgdatetime, pgmath, pgnormalize, pgstring};

/// The prefix of the name of a kernel of a C function.
pub const PREFIX: &str = "__rudb_pgproc_";

/// The C functions of each module of kernels, sorted.
const SOURCES: [&[&str]; 4] =
    [pgmath::SOURCES, pgdatetime::SOURCES, pgstring::SOURCES, pgnormalize::SOURCES];

/// Whether the C function `src` has a kernel.
pub fn has(src: &str) -> bool {
    SOURCES.iter().any(|sources| sources.binary_search(&src).is_ok())
}

/// The C function with a kernel that gives as an array the rows of the C function `src`, which
/// returns a set. A call of `src` is an unnest of that array.
pub fn rows_of(src: &str) -> Option<&'static str> {
    pgstring::ROWS.iter().find(|(rows, _)| *rows == src).map(|&(_, array)| array)
}

/// The value of a call of a kernel of a C function, or `None` for any other name. A null
/// argument of a strict function gives a null. The functions that are not strict, as
/// `proisstrict` has them, see the null.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let Some(src) = name.strip_prefix(PREFIX) else { return Ok(None) };
    if args.iter().any(Value::is_null) && !pgstring::NULLS.contains(&src) {
        return Ok(Some(Value::Null));
    }
    if let Some(value) = pgmath::call(src, args)? {
        return Ok(Some(value));
    }
    if let Some(value) = pgdatetime::call(src, args)? {
        return Ok(Some(value));
    }
    if let Some(value) = pgstring::call(src, args)? {
        return Ok(Some(value));
    }
    pgnormalize::call(src, args)
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
        for sources in SOURCES {
            assert!(sources.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn a_function_of_rows_has_the_strictness_of_its_array_function() {
        for proc in rudb_pgtypes::builtin_procs() {
            if let Some(array) = rows_of(proc.src) {
                assert!(proc.retset && has(array), "{}", proc.src);
                assert_eq!(proc.strict, !pgstring::NULLS.contains(&array), "{}", proc.src);
            }
        }
    }

    #[test]
    fn a_kernel_sees_a_null_when_its_function_is_not_strict() {
        for proc in rudb_pgtypes::builtin_procs() {
            if matches!(proc.lang, b'i' | b'c') && has(proc.src) {
                assert_eq!(proc.strict, !pgstring::NULLS.contains(&proc.src), "{}", proc.src);
            }
        }
    }
}
