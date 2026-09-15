//! `duckdb_databases()` and `duckdb_schemas()`, what is attached and what is in it.
//!
//! The first metadata tables whose rows come out of the catalog rather than out of a list this
//! binary was compiled with. `rudb_functions::entrycatalog` has their columns, because the binder
//! resolves the call and needs the columns before there is a catalog in reach, and the rows are here
//! because this is where one is.
//!
//! Both tables are walked in catalog order, which is the order things were attached and created in.
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

use rudb_catalog::Catalog;
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{DUCKDB, database_fields, schema_fields};
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

/// An empty `MAP(VARCHAR, VARCHAR)`, which is what `tags` and `options` are on every row.
fn empty() -> Value {
    Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new())
}
