//! The four catalog tables: what is attached, what is in it, and what somebody created there.
//!
//! `duckdb_databases()`, `duckdb_schemas()`, `duckdb_tables()` and `duckdb_columns()`, the metadata
//! tables whose rows come out of the catalog rather than out of a list this binary was compiled with.
//! `rudb_functions::entrycatalog` has their columns, because the binder resolves the call and needs
//! the columns before there is a catalog in reach, and the rows are here because this is where one is.
//!
//! `duckdb_views()` is the fifth and it is not here. A view in rudb's catalog is a name and the text
//! of its body, and neither the number of columns it produces nor the deparsed statement the pin
//! reports as `sql` can be worked out from that without binding it. That is a decision about what the
//! catalog stores rather than a row builder, so it is filed and not guessed at. `duckdb_columns()`
//! lists a view's columns upstream and lists only table columns here for the same reason.
//!
//! Every table is walked in catalog order, which is the order things were attached and created in.
//! That is not sorted and it is not reproduced from the pin either, whose order is its own catalog's.
//! A client that wants an order writes one, which is what the two corpus records that read these
//! tables already do.
//!
//! Most of the columns are a fact about a database rudb does not have yet. `path` is null because
//! every database here is in memory, `readonly`, `encrypted` and `cipher` say so, and `options` is
//! empty because `ATTACH` takes none. `internal` is false on the one database rudb attaches, which
//! is the answer upstream gives for `memory` and not the one it gives for `system` and `temp`. The
//! day there is a file behind a database these columns say something.
//!
//! `sql`, `parent_schema` and `parent_schema_oid` are null on every schema row. Upstream's are null
//! too on everything it returns from a fresh session, because a schema created by `CREATE SCHEMA`
//! has no stored text and nothing nests schemas.

use rudb_catalog::{Catalog, Database, Schema, Table};
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{
    DUCKDB, canonical, column_fields, database_fields, numeric_facts, schema_fields, table_fields,
    type_oid,
};
use rudb_parse::quoted;
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every attached database, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn databasenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(catalog.databases().len());
    for database in catalog.databases() {
        rows.push(vec![
            text(database.name()),
            Value::BigInt(database.oid()),
            Value::Null,
            Value::Null,
            empty(),
            Value::Boolean(false),
            text(DUCKDB),
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Null,
            empty(),
        ]);
    }
    Metadata::new("duckdb_databases", &database_fields(), &rows, plan, index, columns)
}

/// Every schema in every attached database, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn schemanames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            rows.push(vec![
                Value::BigInt(schema.oid()),
                text(database.name()),
                Value::BigInt(database.oid()),
                text(schema.name()),
                Value::Null,
                empty(),
                // True on every schema upstream returns from a fresh session, including `memory.main`
                // which is the one rudb has. A schema somebody made with `CREATE SCHEMA` is false
                // there, and the day rudb can tell the two apart this stops being a constant.
                Value::Boolean(true),
                Value::Null,
                Value::Null,
                Value::Null,
            ]);
        }
    }
    Metadata::new("duckdb_schemas", &schema_fields(), &rows, plan, index, columns)
}

/// Every base table in the catalog, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn tablenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for (database, schema, table) in entries(catalog) {
        let count = i64::try_from(table.columns().len()).unwrap_or(i64::MAX);
        rows.push(vec![
            text(database.name()),
            Value::BigInt(database.oid()),
            text(schema.name()),
            Value::BigInt(schema.oid()),
            text(&table.name().table),
            Value::BigInt(table.oid()),
            Value::Null,
            empty(),
            Value::Boolean(false),
            // Neither of these can be true yet. `CREATE TEMP TABLE` is not a statement rudb takes
            // and a primary key is not a constraint it stores, so both are a constant rather than a
            // fact read off the entry, and both stop being one the day the DDL grows the clause.
            Value::Boolean(false),
            Value::Boolean(false),
            Value::BigInt(i64::try_from(table.rows().len()).unwrap_or(i64::MAX)),
            Value::BigInt(count),
            Value::BigInt(0),
            Value::BigInt(0),
            text(&create_table(table)),
        ]);
    }
    Metadata::new("duckdb_tables", &table_fields(), &rows, plan, index, columns)
}

/// Every column of every base table, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn columnnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for (database, schema, table) in entries(catalog) {
        for (at, column) in table.columns().iter().enumerate() {
            let (precision, radix, scale) = numeric_facts(&column.ty);
            rows.push(vec![
                text(database.name()),
                Value::BigInt(database.oid()),
                text(schema.name()),
                Value::BigInt(schema.oid()),
                text(&table.name().table),
                Value::BigInt(table.oid()),
                text(&column.name),
                // One based, which is the pin's answer and not the position in the vector.
                Value::Integer(i32::try_from(at + 1).unwrap_or(i32::MAX)),
                Value::Null,
                Value::Boolean(false),
                Value::Null,
                Value::Boolean(!column.not_null),
                text(&column.ty.to_string()),
                type_oid(&canonical(&column.ty)).map_or(Value::Null, Value::BigInt),
                // Null even on a VARCHAR the DDL gave a length, because the pin reports null there
                // too: DuckDB parses the length modifier and then drops it, so by the time a column
                // is in a catalog there is no length left to report.
                Value::Null,
                precision.map_or(Value::Null, Value::Integer),
                radix.map_or(Value::Null, Value::Integer),
                scale.map_or(Value::Null, Value::Integer),
                empty(),
                Value::Boolean(false),
                Value::Null,
            ]);
        }
    }
    Metadata::new("duckdb_columns", &column_fields(), &rows, plan, index, columns)
}

/// Every base table in the catalog with the database and schema it is in.
fn entries(catalog: &Catalog) -> impl Iterator<Item = (&Database, &Schema, &Table)> {
    catalog.databases().iter().flat_map(|database| {
        database.schemas().iter().flat_map(move |schema| {
            schema.tables().iter().map(move |table| (database, schema, table))
        })
    })
}

/// The `CREATE TABLE` a table would be made by, which is what `duckdb_tables()` reports as `sql`.
///
/// Written back out rather than stored. The pin does the same, which is measured: a table created
/// with odd spacing and lower case type names comes back normalised, so the column is a deparse of
/// the entry and not the text somebody typed. Identifiers go through [`rudb_parse::quoted`], which
/// is the rule the binder already uses for a generated column name.
fn create_table(table: &Table) -> String {
    let columns: Vec<String> = table
        .columns()
        .iter()
        .map(|column| {
            let null = if column.not_null { " NOT NULL" } else { "" };
            format!("{} {}{null}", quoted(&column.name), column.ty)
        })
        .collect();
    format!("CREATE TABLE {}({});", quoted(&table.name().table), columns.join(", "))
}

/// An empty `MAP(VARCHAR, VARCHAR)`, which is what `tags` and `options` are on every row.
fn empty() -> Value {
    Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new())
}
