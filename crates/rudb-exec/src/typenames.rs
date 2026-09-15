//! `duckdb_types()`, every type name the engine knows and what each one stands for.
//!
//! The list itself is `rudb_functions::TYPE_NAMES`, because which names exist and which modifiers
//! each one takes is a fact about the type system rather than about this operator. What is here is
//! the seventeen columns, and three of them say something about rudb rather than repeating the pin.
//!
//! `database_oid` and `schema_oid` are null. The pin fills them with a counter its catalog handed
//! out at startup, so on the pinned binary they are 20732 and 20730 and on the next build of it they
//! are something else. rudb has no oid space for catalogs and schemas at all, and inventing one so
//! that a column could hold a number nobody can rely on would be worse than saying there isn't one.
//! `type_oid` is a different matter and is filled in, because that one is `LogicalTypeId` and is
//! stable across builds.
//!
//! `type_size` is rudb's layout rather than the pin's, and the three places they differ are written
//! down on `PhysicalType::size`. A client that reads this column wants to know what this engine
//! stores, so reporting somebody else's number would be a table that lies in the one column that is
//! about bytes.
//!
//! Everything else matches: `comment` and `extension_name` and `labels` are null, `tags` is the
//! empty map, `internal` is true on every row because every name here is built in, and `parameters`
//! and `parameter_types` are empty lists rather than null on a row with no modifiers.

use rudb_catalog::{DEFAULT_CATALOG, DEFAULT_SCHEMA};
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{TYPE_NAMES, type_category, type_fields, type_size};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every type name and every modifier signature it takes, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn typenames(plan: &Plan, index: u32, columns: Slice) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(TYPE_NAMES.len());
    for entry in TYPE_NAMES {
        for (position, signature) in entry.signatures.iter().enumerate() {
            let parameters = signature.iter().map(|(name, _)| text(name)).collect();
            let types = signature.iter().map(|(_, ty)| text(ty)).collect();
            rows.push(vec![
                text(DEFAULT_CATALOG),
                Value::Null,
                text(DEFAULT_SCHEMA),
                Value::Null,
                // The oid an entry carries belongs to the type and the type has one bare row, so it
                // goes on the first signature and nowhere else. Every entry that carries one has a
                // bare signature first, which `the_oid_sits_on_the_first_name_of_its_type` is what
                // keeps true.
                match entry.oid {
                    Some(oid) if position == 0 => Value::BigInt(oid),
                    _ => Value::Null,
                },
                text(entry.name),
                type_size(entry.logical_type).map_or(Value::Null, Value::BigInt),
                text(entry.logical_type),
                type_category(entry.logical_type).map_or(Value::Null, text),
                Value::Null,
                Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new()),
                Value::Boolean(true),
                Value::Null,
                Value::Null,
                Value::List { element: LogicalType::Varchar, values: parameters },
                Value::List { element: LogicalType::Varchar, values: types },
                entry.varargs.map_or(Value::Null, text),
            ]);
        }
    }
    Metadata::new("duckdb_types", &type_fields(), &rows, plan, index, columns)
}
