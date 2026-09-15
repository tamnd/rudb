//! The columns of the tables that describe what somebody created.
//!
//! `duckdb_databases()` and `duckdb_schemas()` here, and the three that describe a table are the
//! next piece of D2. Only the columns are here, because unlike the other four metadata tables the
//! rows are not a fact about the binary. They are whatever is in the catalog, so they are built in
//! `rudb_exec` where the catalog is in reach and this crate is only the half both ends agree on.
//!
//! # Every one of these tables has an oid column and rudb fills them in
//!
//! `duckdb_types()` reports `database_oid` as null and says so in its own doc, because upstream's is
//! a counter its catalog handed out at startup and reproducing an accident of one process's ordering
//! is not compatibility. These tables are the other case. A tool reads `duckdb_columns()` and joins
//! it to `duckdb_tables()` on `table_oid`, and a null there is not a small divergence, it is the
//! table failing at the one job it has. So the catalog hands out its own oids and these report them.
//! The numbers will not be upstream's and they are not meant to be. What has to hold is that the
//! same entry carries the same number in every table that names it, and that no number is handed out
//! twice.
//!
//! # What rudb has fewer of
//!
//! Upstream returns three databases on a fresh in memory session, `memory`, `system` and `temp`, and
//! five schemas. rudb has `memory.main` and nothing else, so it returns one of each. `system` holds
//! the builtins and `temp` holds what `CREATE TEMP TABLE` makes, and rudb has neither an attached
//! catalog for its builtins nor a temporary one. `duckdb_functions()` already reports `system.main`
//! for every function it lists, which is a name `duckdb_schemas()` does not return, and that
//! disagreement is real rather than an oversight here. It is filed rather than papered over.

use rudb_common::{Field, LogicalType};

/// The `type` column of `duckdb_databases()`, which says what is behind an attached name.
pub const DUCKDB: &str = "duckdb";

/// The columns `duckdb_databases()` returns, in the pin's order.
#[must_use]
pub fn database_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("path", LogicalType::Varchar),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", tags()),
        Field::new("internal", LogicalType::Boolean),
        Field::new("type", LogicalType::Varchar),
        Field::new("readonly", LogicalType::Boolean),
        Field::new("encrypted", LogicalType::Boolean),
        Field::new("cipher", LogicalType::Varchar),
        Field::new("options", tags()),
    ]
}

/// The columns `duckdb_schemas()` returns, in the pin's order.
///
/// `oid` first and unqualified, which is this table alone. Every other one spells the column after
/// what it names, so a query that reads several of them has to remember that the schema's own oid is
/// `oid` here and `schema_oid` everywhere else.
#[must_use]
pub fn schema_fields() -> Vec<Field> {
    vec![
        Field::new("oid", LogicalType::BigInt),
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", tags()),
        Field::new("internal", LogicalType::Boolean),
        Field::new("sql", LogicalType::Varchar),
        Field::new("parent_schema", LogicalType::Varchar),
        Field::new("parent_schema_oid", LogicalType::BigInt),
    ]
}

/// The `MAP(VARCHAR, VARCHAR)` that every one of these tables carries at least one of.
fn tags() -> LogicalType {
    LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)
}

#[cfg(test)]
mod tests {
    use super::{database_fields, schema_fields};

    #[test]
    fn the_two_tables_are_the_shape_the_pin_returns() {
        assert_eq!(database_fields().len(), 11);
        assert_eq!(schema_fields().len(), 10);
    }

    /// The one column name that does not follow the rule the other four tables follow.
    #[test]
    fn a_schemas_own_oid_is_spelled_oid_and_not_schema_oid() {
        assert_eq!(schema_fields()[0].name, "oid");
        assert!(schema_fields().iter().all(|field| field.name != "schema_oid"));
    }
}
