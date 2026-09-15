//! `duckdb_functions()`, every function the engine knows and what each one takes.
//!
//! The list is `rudb_functions::function_entries`, because which functions exist and what each one
//! accepts is a fact about the function library rather than about this operator. What is here is the
//! twenty one columns.
//!
//! Six of them are null on every row and each one is null for a reason worth saying once.
//! `description`, `comment`, `examples` and `categories` are null because rudb has no documentation
//! strings attached to its functions, and writing a sentence per name in here would put the
//! documentation in a file nobody reads when they change a function. `macro_definition` is null
//! because rudb has no macros. `function_oid` is null because upstream's is a counter its catalog
//! handed out at startup and rudb has no oid space, which is the same answer `duckdb_types()` gives
//! for `database_oid`.
//!
//! `internal` is true on every row, because every function rudb has is built in. A user defined one
//! arrives with a row that says false, and the day it does the two corpus records that read this
//! table with `where not internal` start meaning something.

use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{FUNCTION_CATALOG, FUNCTION_SCHEMA, function_entries, function_fields};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every function and every argument count it takes, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn functionnames(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let entries = function_entries();
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        rows.push(vec![
            text(FUNCTION_CATALOG),
            Value::Null,
            text(FUNCTION_SCHEMA),
            text(entry.name),
            entry.alias_of.map_or(Value::Null, text),
            text(entry.function_type),
            Value::Null,
            Value::Null,
            Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new()),
            entry.return_type.map_or(Value::Null, text),
            strings(&entry.parameters),
            strings(&entry.parameter_types),
            entry.varargs.map_or(Value::Null, text),
            Value::Null,
            entry.has_side_effects.map_or(Value::Null, Value::Boolean),
            Value::Boolean(true),
            Value::Null,
            Value::Null,
            Value::Null,
            entry.stability.map_or(Value::Null, text),
            Value::Null,
        ]);
    }
    Metadata::new("duckdb_functions", &function_fields(), &rows, plan, index, columns)
}

/// A `VARCHAR[]` holding these, which is what three of the columns are.
fn strings(values: &[String]) -> Value {
    Value::List {
        element: LogicalType::Varchar,
        values: values.iter().map(|value| Value::Varchar(value.clone())).collect(),
    }
}
