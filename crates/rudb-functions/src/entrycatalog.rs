//! The columns of the tables that describe what somebody created.
//!
//! `duckdb_databases()`, `duckdb_schemas()`, `duckdb_tables()`, `duckdb_views()` and
//! `duckdb_columns()`. Only the columns are here, because unlike the other metadata tables the rows
//! are not a fact about the binary. They are whatever is in the catalog, so they are built in
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

/// The columns `duckdb_tables()` returns, in the pin's order.
#[must_use]
pub fn table_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("schema_oid", LogicalType::BigInt),
        Field::new("table_name", LogicalType::Varchar),
        Field::new("table_oid", LogicalType::BigInt),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", tags()),
        Field::new("internal", LogicalType::Boolean),
        Field::new("temporary", LogicalType::Boolean),
        Field::new("has_primary_key", LogicalType::Boolean),
        Field::new("estimated_size", LogicalType::BigInt),
        Field::new("column_count", LogicalType::BigInt),
        Field::new("index_count", LogicalType::BigInt),
        Field::new("check_constraint_count", LogicalType::BigInt),
        Field::new("sql", LogicalType::Varchar),
    ]
}

/// The columns `duckdb_views()` returns, in the pin's order.
///
/// Thirteen, which is three fewer than `duckdb_tables()` and not the same thirteen. There is no
/// `estimated_size` and no `index_count` because a view holds nothing, and there is an `is_bound`
/// which says whether the column cache on the entry holds anything yet.
#[must_use]
pub fn view_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("schema_oid", LogicalType::BigInt),
        Field::new("view_name", LogicalType::Varchar),
        Field::new("view_oid", LogicalType::BigInt),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", tags()),
        Field::new("internal", LogicalType::Boolean),
        Field::new("temporary", LogicalType::Boolean),
        Field::new("column_count", LogicalType::BigInt),
        Field::new("sql", LogicalType::Varchar),
        Field::new("is_bound", LogicalType::Boolean),
    ]
}

/// The one column `PRAGMA show_tables` returns.
///
/// Named `name` and nothing else, because the statement answers what is in reach of an unqualified
/// name and a client that wants to know where each one lives asks `PRAGMA show_tables_expanded`.
#[must_use]
pub fn show_table_fields() -> Vec<Field> {
    vec![Field::new("name", LogicalType::Varchar)]
}

/// The one column `PRAGMA show_databases` returns.
///
/// `database_name` rather than `name`, which is the spelling `duckdb_databases()` uses as well, and
/// it differs from the column `PRAGMA show_tables` returns for no reason either of them states.
#[must_use]
pub fn show_database_fields() -> Vec<Field> {
    vec![Field::new("database_name", LogicalType::Varchar)]
}

/// The six columns `PRAGMA show_tables_expanded` returns.
///
/// The column names and the column types are two `VARCHAR[]` down one row rather than a row each,
/// which makes this the one catalog table that answers a table's shape without a join. `temporary`
/// is the last column and it is the only one that is not a name.
#[must_use]
pub fn show_expanded_fields() -> Vec<Field> {
    vec![
        Field::new("database", LogicalType::Varchar),
        Field::new("schema", LogicalType::Varchar),
        Field::new("name", LogicalType::Varchar),
        Field::new("column_names", LogicalType::list(LogicalType::Varchar)),
        Field::new("column_types", LogicalType::list(LogicalType::Varchar)),
        Field::new("temporary", LogicalType::Boolean),
    ]
}

/// The columns `duckdb_columns()` returns, in the pin's order.
#[must_use]
pub fn column_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_oid", LogicalType::BigInt),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("schema_oid", LogicalType::BigInt),
        Field::new("table_name", LogicalType::Varchar),
        Field::new("table_oid", LogicalType::BigInt),
        Field::new("column_name", LogicalType::Varchar),
        Field::new("column_index", LogicalType::Integer),
        Field::new("comment", LogicalType::Varchar),
        Field::new("internal", LogicalType::Boolean),
        Field::new("column_default", LogicalType::Varchar),
        Field::new("is_nullable", LogicalType::Boolean),
        Field::new("data_type", LogicalType::Varchar),
        Field::new("data_type_id", LogicalType::BigInt),
        Field::new("character_maximum_length", LogicalType::Integer),
        Field::new("numeric_precision", LogicalType::Integer),
        Field::new("numeric_precision_radix", LogicalType::Integer),
        Field::new("numeric_scale", LogicalType::Integer),
        Field::new("tags", tags()),
        Field::new("is_generated", LogicalType::Boolean),
        Field::new("generation_expression", LogicalType::Varchar),
    ]
}

/// The canonical name of a type, which is what [`crate::typecatalog::type_oid`] is keyed by.
///
/// The type written out, minus whatever modifiers it carries. `DECIMAL(9,2)` and `DECIMAL(38,10)`
/// are both the same type as far as `data_type_id` is concerned, and so are a list of integers and a
/// list of strings, because the oid is `LogicalTypeId` and that enumeration has one entry for the
/// type constructor rather than one per instance of it.
#[must_use]
pub fn canonical(ty: &LogicalType) -> String {
    match ty {
        LogicalType::Decimal { .. } => "DECIMAL".to_string(),
        LogicalType::List(_) | LogicalType::Array(_, _) => "LIST".to_string(),
        LogicalType::Map(_, _) => "MAP".to_string(),
        LogicalType::Struct(_) => "STRUCT".to_string(),
        LogicalType::Union(_) => "UNION".to_string(),
        other => other.to_string(),
    }
}

/// The three numeric columns of `duckdb_columns()`, which most types report nothing in.
///
/// Measured off the pin rather than reasoned about, because the answers are not what the column
/// names suggest. `numeric_precision` on an integer is a count of bits and not of digits, so an
/// `INTEGER` reports 32 with a radix of 2, and a `FLOAT` reports 24 and a `DOUBLE` 53 because those
/// are the mantissa widths. A `DECIMAL` is the one type where the number means digits, so it reports
/// its width with a radix of 10 and its scale. Every unsigned integer reports nothing at all, which
/// looks like an oversight upstream and is reproduced here because a client reading these is reading
/// them from DuckDB's side of the comparison.
#[must_use]
pub fn numeric_facts(ty: &LogicalType) -> (Option<i32>, Option<i32>, Option<i32>) {
    let binary = |bits| (Some(bits), Some(2), Some(0));
    match ty {
        LogicalType::TinyInt => binary(8),
        LogicalType::SmallInt => binary(16),
        LogicalType::Integer => binary(32),
        LogicalType::BigInt => binary(64),
        LogicalType::HugeInt => binary(128),
        LogicalType::Float => binary(24),
        LogicalType::Double => binary(53),
        LogicalType::Decimal { width, scale } => {
            (Some(i32::from(*width)), Some(10), Some(i32::from(*scale)))
        }
        _ => (None, None, None),
    }
}

/// The `MAP(VARCHAR, VARCHAR)` that every one of these tables carries at least one of.
fn tags() -> LogicalType {
    LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::{
        canonical, column_fields, database_fields, numeric_facts, schema_fields, table_fields,
        view_fields,
    };

    #[test]
    fn the_five_tables_are_the_shape_the_pin_returns() {
        assert_eq!(database_fields().len(), 11);
        assert_eq!(schema_fields().len(), 10);
        assert_eq!(table_fields().len(), 16);
        assert_eq!(view_fields().len(), 13);
        assert_eq!(column_fields().len(), 21);
    }

    /// The four values read off the pin, which are not the ones the column names suggest.
    #[test]
    fn a_numeric_precision_is_bits_everywhere_except_on_a_decimal() {
        assert_eq!(numeric_facts(&LogicalType::Integer), (Some(32), Some(2), Some(0)));
        assert_eq!(numeric_facts(&LogicalType::Double), (Some(53), Some(2), Some(0)));
        assert_eq!(
            numeric_facts(&LogicalType::Decimal { width: 9, scale: 2 }),
            (Some(9), Some(10), Some(2))
        );
        // An unsigned integer reports nothing, which is upstream's answer and not an omission here.
        assert_eq!(numeric_facts(&LogicalType::UBigInt), (None, None, None));
        assert_eq!(numeric_facts(&LogicalType::Varchar), (None, None, None));
    }

    #[test]
    fn a_types_modifiers_are_not_part_of_the_name_the_oid_is_keyed_by() {
        assert_eq!(canonical(&LogicalType::Decimal { width: 9, scale: 2 }), "DECIMAL");
        assert_eq!(canonical(&LogicalType::list(LogicalType::Integer)), "LIST");
        assert_eq!(canonical(&LogicalType::Integer), "INTEGER");
        assert_eq!(canonical(&LogicalType::TimestampTz), "TIMESTAMP WITH TIME ZONE");
    }

    /// The one column name that does not follow the rule the other four tables follow.
    #[test]
    fn a_schemas_own_oid_is_spelled_oid_and_not_schema_oid() {
        assert_eq!(schema_fields()[0].name, "oid");
        assert!(schema_fields().iter().all(|field| field.name != "schema_oid"));
    }
}
