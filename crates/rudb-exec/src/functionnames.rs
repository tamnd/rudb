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
//! because the built in macros here are not listed with their bodies. `function_oid` is null
//! because upstream's is a counter its catalog handed out at startup and rudb has no oid space for
//! its built in functions, which is the same answer `duckdb_types()` gives for `database_oid`.
//!
//! `internal` is true on every built in row. A macro a user made comes after them with a row that
//! says false, its body in `macro_definition`, its oid, and the empty lists the pin gives it.

use rudb_catalog::Catalog;
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{FUNCTION_CATALOG, FUNCTION_SCHEMA, function_entries, function_fields};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every function and every argument count it takes, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn functionnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
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
    for database in catalog.databases() {
        for schema in database.schemas() {
            for made in schema.macros() {
                let kind = if made.table { "table_macro" } else { "macro" };
                for overload in &made.overloads {
                    let names: Vec<String> = overload
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name.clone())
                        .collect();
                    let types = Value::List {
                        element: LogicalType::Varchar,
                        values: overload
                            .parameters
                            .iter()
                            .map(|parameter| {
                                parameter.ty.clone().map_or(Value::Null, Value::Varchar)
                            })
                            .collect(),
                    };
                    rows.push(vec![
                        text(database.name()),
                        text(&database.oid().to_string()),
                        text(schema.name()),
                        text(&made.name.table),
                        Value::Null,
                        text(kind),
                        Value::Null,
                        Value::Null,
                        Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new()),
                        Value::Null,
                        strings(&names),
                        types,
                        Value::Null,
                        text(&overload.body),
                        Value::Null,
                        Value::Boolean(false),
                        Value::Null,
                        Value::BigInt(made.oid),
                        strings(&[]),
                        Value::Null,
                        strings(&[]),
                    ]);
                }
            }
        }
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
