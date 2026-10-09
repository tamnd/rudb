//! PostgreSQL's errors for a call whose `OVER` or null treatment does not fit the function, as
//! `ParseFuncOrColumn` in `parse_func.c` gives them.
//!
//! PostgreSQL finds the function first and then looks at what was written after it. A function
//! that is neither an aggregate nor a window function cannot have an `OVER` or a null treatment,
//! an aggregate cannot have a null treatment even inside an `OVER`, and a window function cannot
//! be called without an `OVER`. A name it does not find over the arguments is the error of a
//! function that does not exist, whatever was written after the call.
//!
//! Of the window functions, only `lag`, `lead`, `first_value`, `last_value` and `nth_value` read
//! values and so have nulls to skip. The others refuse a null treatment. PostgreSQL does that
//! when the function computes its first row, so the error has no position, and a query over no
//! rows does not get it there. It is said here when the call is bound.
//!
//! `count()` is the pin's other spelling of `count(*)`, and PostgreSQL refuses it with or without
//! an `OVER`, since it wants the star for an aggregate that takes no arguments.

use rudb_common::{Error, LogicalType, SqlState};
use rudb_functions::{FunctionKind, kind_of};
use rudb_parse::ast::{self, Ast};

use crate::orderedset::undefined;
use crate::pgcalls::operand_oids;

/// The window functions that take `RESPECT NULLS` and `IGNORE NULLS`.
const TREATS_NULLS: [&str; 5] = ["lag", "lead", "first_value", "last_value", "nth_value"];

/// PostgreSQL's error for `written(arguments) OVER (...)` when `error` is what the pin says: the
/// name is not an aggregate or a window function, or no form of it takes these arguments.
///
/// A plain function that PostgreSQL finds over the arguments is refused for its kind, and a
/// name it does not find is the error of a function that does not exist. Anything else keeps the
/// error of the pin.
pub(crate) fn not_windowed(
    ast: &Ast,
    call: ast::ExprRef,
    written: &str,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
    error: Error,
) -> Error {
    use rudb_pgtypes::Resolution;
    let Some(oids) = operand_oids(ast, arguments, types) else {
        return error;
    };
    match rudb_pgtypes::resolve_function(written, &oids) {
        Resolution::Found(found) if found.proc.kind == b'f' => refused(format!(
            "OVER specified, but {written} is not a window function nor an aggregate function"
        )),
        Resolution::NotFound(_) => undefined(ast, call, written, arguments, types),
        _ => error,
    }
}

/// PostgreSQL's error for a null treatment on a call with an `OVER`, or `None` for one of the
/// window functions that take it.
pub(crate) fn treated_window(written: &str) -> Option<Error> {
    match kind_of(written) {
        Some(FunctionKind::Aggregate) => Some(refused_aggregate()),
        _ if TREATS_NULLS.iter().any(|name| rudb_catalog::same_name(name, written)) => None,
        _ => Some(
            Error::not_implemented(format!(
                "function {written} does not allow RESPECT/IGNORE NULLS"
            ))
            .state(SqlState::FEATURE_NOT_SUPPORTED)
            .unplaced(),
        ),
    }
}

/// PostgreSQL's error for a null treatment on a call with no `OVER` that is found. A window
/// function is refused for the missing `OVER` before this, where the pin refuses it too.
pub(crate) fn treated_call(written: &str) -> Error {
    let aggregate = kind_of(written) == Some(FunctionKind::Aggregate)
        || rudb_pgtypes::procs(written).iter().any(|proc| proc.kind == b'a');
    if aggregate {
        return refused_aggregate();
    }
    refused(format!("RESPECT/IGNORE NULLS specified, but {written} is not a window function"))
}

/// PostgreSQL's error for `count()`, an aggregate with no arguments written without the star.
pub(crate) fn parameterless(written: &str) -> Error {
    refused(format!("{written}(*) must be used to call a parameterless aggregate function"))
}

/// PostgreSQL's error for a window function written with no `OVER`.
pub(crate) fn unwindowed(written: &str) -> String {
    format!("window function {written} requires an OVER clause")
}

fn refused_aggregate() -> Error {
    refused("aggregate functions do not accept RESPECT/IGNORE NULLS".to_string())
}

fn refused(message: String) -> Error {
    Error::binder(message).state(SqlState::WRONG_OBJECT_TYPE)
}
