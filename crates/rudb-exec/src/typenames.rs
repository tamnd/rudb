//! `duckdb_types()`, every type name the engine knows and what each one stands for.
//!
//! The list itself is `rudb_functions::TYPE_NAMES`, because which names exist and which modifiers
//! each one takes is a fact about the type system rather than about this operator. What is here is
//! the seventeen columns, and three of them say something about rudb rather than repeating the pin.
//!
//! The built in names are listed once for every attached database, under its `main` schema and
//! with the oids of the two, which is what the pin does and what `pg_catalog.pg_type` reads to put
//! a type in a namespace. The databases come by name, and the schemas of each by name. Only the
//! first database listed has a `type_oid` on its built in rows, as on the pin.
//! `type_oid` is `LogicalTypeId` and is stable across builds.
//!
//! `type_size` is rudb's layout rather than the pin's, and the three places they differ are written
//! down on `PhysicalType::size`. A client that reads this column wants to know what this engine
//! stores, so reporting somebody else's number would be a table that lies in the one column that is
//! about bytes.
//!
//! Everything else matches: `comment` and `extension_name` and `labels` are null, `tags` is the
//! empty map, `internal` is true on every built in row, and `parameters` and `parameter_types` are
//! empty lists rather than null on a row with no modifiers.
//!
//! The types `CREATE TYPE` made sit among the built in ones of their schema, in the order of their
//! names with case folded up, which is the order the pin lists them in. Those rows are not
//! internal, and they carry an oid of their own, as a table does.

use rudb_catalog::{Catalog, DEFAULT_SCHEMA};
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{TYPE_NAMES, canonical, type_category, type_fields, type_size};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every type name and every modifier signature it takes, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn typenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut databases: Vec<_> = catalog.databases().iter().collect();
    databases.sort_by_key(|database| database.name().to_lowercase());
    let mut rows = Vec::with_capacity(TYPE_NAMES.len() * databases.len());
    for (listed, database) in databases.into_iter().enumerate() {
        let mut schemas: Vec<_> = database.schemas().iter().collect();
        schemas.sort_by_key(|schema| schema.name().to_lowercase());
        for schema in schemas {
            let mut named: Vec<(String, Vec<Value>)> = Vec::new();
            let place = vec![
                text(database.name()),
                Value::BigInt(database.oid()),
                text(schema.name()),
                Value::BigInt(schema.oid()),
            ];
            if schema.name() == DEFAULT_SCHEMA {
                for entry in TYPE_NAMES {
                    for (position, signature) in entry.signatures.iter().enumerate() {
                        let parameters = signature.iter().map(|(name, _)| text(name)).collect();
                        let types = signature.iter().map(|(_, ty)| text(ty)).collect();
                        let mut row = place.clone();
                        row.extend([
                            // The oid an entry carries belongs to the type and the type has one
                            // bare row, so it goes on the first signature and nowhere else. Every
                            // entry that carries one has a bare signature first, which
                            // `the_oid_sits_on_the_first_name_of_its_type` is what keeps true. The
                            // pin puts it in the first database it lists and leaves it null in the
                            // others, which is what keeps `pg_type` to one row an oid.
                            match entry.oid {
                                Some(oid) if position == 0 && listed == 0 => Value::BigInt(oid),
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
                        named.push((entry.name.to_uppercase(), row));
                    }
                }
            }
            let made = catalog.types().filter(|held| {
                held.name().catalog == database.name() && held.name().schema == schema.name()
            });
            for held in made {
                let logical_type = canonical(held.ty());
                // An enum is the one made type with something to say in `labels`, and the one
                // whose size is its own rather than its kind's, since it is as wide as its list.
                let (size, labels) = match held.ty().labels() {
                    Some(labels) => (
                        i64::try_from(held.ty().physical().size()).ok(),
                        Value::List {
                            element: LogicalType::Varchar,
                            values: labels.iter().map(|label| text(label)).collect(),
                        },
                    ),
                    None => (type_size(&logical_type), Value::Null),
                };
                let mut row = place.clone();
                row.extend([
                    Value::BigInt(held.oid()),
                    text(&held.name().table),
                    size.map_or(Value::Null, Value::BigInt),
                    text(&logical_type),
                    type_category(&logical_type).map_or(Value::Null, text),
                    Value::Null,
                    Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new()),
                    Value::Boolean(false),
                    Value::Null,
                    labels,
                    Value::List { element: LogicalType::Varchar, values: Vec::new() },
                    Value::List { element: LogicalType::Varchar, values: Vec::new() },
                    Value::Null,
                ]);
                named.push((held.name().table.to_uppercase(), row));
            }
            // Stable, so the signatures of one name keep the order the list gives them.
            named.sort_by(|a, b| a.0.cmp(&b.0));
            rows.extend(named.into_iter().map(|(_, row)| row));
        }
    }
    Metadata::new("duckdb_types", &type_fields(), &rows, plan, index, columns)
}
