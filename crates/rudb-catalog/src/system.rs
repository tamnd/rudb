//! The catalogs a session has without attaching anything, and the views upstream puts in them.
//!
//! A fresh DuckDB session has three databases and not one. `memory` is the one a person creates in.
//! `system` holds everything the engine ships with, in three schemas: `main` for the wrappers that
//! hide internal rows, `information_schema` for the standard views, and `pg_catalog` for the
//! Postgres ones. `temp` holds what `CREATE TEMP` makes. All of that was measured on the pin, where
//! `duckdb_databases()` returns three rows and `duckdb_schemas()` returns five.
//!
//! # The views are SQL upstream wrote, not a table of rows
//!
//! Every one of the 47 internal views is an ordinary view over the `duckdb_*` table functions, and
//! upstream reports the whole statement back through `duckdb_views().sql`, so the body of each one
//! is readable from the pin rather than guessable. That is what is written down here: the text is
//! theirs, copied off `information_schema.views.view_definition`, which is why these read like
//! somebody else's formatting. Adopting the body means the rows agree by construction instead of
//! agreeing because two implementations of the same idea were compared row by row.
//!
//! The body is bound at the first reference like any other view, so the cost of having these in
//! every session is the strings and nothing else, and a session that never reads one never binds it.
//! That is also what the pin does: a fresh session reports `is_bound` false and a null
//! `column_count` for all 47 of them, and reading one fills both in.
//!
//! # What is here and what is not
//!
//! Twelve of the 47, which are the ones whose bodies only read table functions rudb has. The other
//! thirty five are waiting on `duckdb_constraints()`, `duckdb_indexes()`, `duckdb_logs()`, `unnest`,
//! `generate_series` and lambdas, and the six `information_schema` constraint views are the ones a
//! client is most likely to miss. Nothing here is a rudb invention: a view is either upstream's own
//! text or it is absent, so a client that finds one can trust it answers the way the pin does.

/// The database the engine's own entries live in.
pub const SYSTEM_CATALOG: &str = "system";
/// The database `CREATE TEMP` puts things in.
pub const TEMP_CATALOG: &str = "temp";
/// The schema in `system` that holds the standard views.
pub const INFORMATION_SCHEMA: &str = "information_schema";
/// The schema in `system` that holds the Postgres views.
pub const PG_CATALOG: &str = "pg_catalog";

/// One view the engine ships with.
pub(crate) struct Internal {
    /// The schema in `system` it goes in.
    pub(crate) schema: &'static str,
    /// The name it answers to.
    pub(crate) name: &'static str,
    /// The name as the statement writes it, which carries the schema when upstream's does and the
    /// double quotes when the name is a keyword.
    pub(crate) written: &'static str,
    /// The body, which is what the binder binds at every reference.
    pub(crate) sql: &'static str,
}

/// The whole statement, which is what `duckdb_views()` reports and what a client reads to find out
/// what a view means.
///
/// Built rather than stored, because the two ways of writing the same view would otherwise be two
/// strings that can drift apart. `CREATE TEMP VIEW` is upstream's spelling for these and it is a
/// little odd, since they end up in `system` rather than in `temp`, but it is what the pin prints.
pub(crate) fn statement(view: &Internal) -> String {
    format!("CREATE TEMP VIEW {} AS {};", view.written, view.sql)
}

/// The views a fresh session has, in the order they go into the catalog.
pub(crate) const INTERNAL_VIEWS: &[Internal] = &[
    // The wrappers. Each one is the table function of the same name with the engine's own rows
    // taken out, and the pair is the reason `duckdb_tables` and `duckdb_tables()` are different
    // questions: the parentheses ask what is really there and the bare name asks what somebody
    // made. Two of the bodies below read the wrapper for exactly that reason.
    Internal {
        schema: "main",
        name: "duckdb_columns",
        written: "duckdb_columns",
        sql: "SELECT * FROM duckdb_columns() WHERE (NOT internal)",
    },
    Internal {
        schema: "main",
        name: "duckdb_databases",
        written: "duckdb_databases",
        sql: "SELECT * FROM duckdb_databases() WHERE (NOT internal)",
    },
    Internal {
        schema: "main",
        name: "duckdb_schemas",
        written: "duckdb_schemas",
        sql: "SELECT * FROM duckdb_schemas() WHERE (NOT internal)",
    },
    Internal {
        schema: "main",
        name: "duckdb_tables",
        written: "duckdb_tables",
        sql: "SELECT * FROM duckdb_tables() WHERE (NOT internal)",
    },
    Internal {
        schema: "main",
        name: "duckdb_types",
        written: "duckdb_types",
        sql: "SELECT * FROM duckdb_types()",
    },
    Internal {
        schema: "main",
        name: "duckdb_views",
        written: "duckdb_views",
        sql: "SELECT * FROM duckdb_views() WHERE (NOT internal)",
    },
    // The one member of the `pragma_*` family that is a view rather than a table function, which is
    // SQLite's `PRAGMA database_list` under the name a query can read it by.
    Internal {
        schema: "main",
        name: "pragma_database_list",
        written: "pragma_database_list",
        sql: "SELECT database_oid AS seq, database_name AS \"name\", path AS file \
              FROM duckdb_databases() WHERE (NOT internal) ORDER BY 1",
    },
    // The standard views. Five of the eleven, and the six that are missing are the constraint ones.
    Internal {
        schema: INFORMATION_SCHEMA,
        name: "character_sets",
        written: "information_schema.character_sets",
        sql: "SELECT CAST(NULL AS VARCHAR) AS character_set_catalog, \
              CAST(NULL AS VARCHAR) AS character_set_schema, 'UTF8' AS character_set_name, \
              'UCS' AS character_repertoire, 'UTF8' AS form_of_use, \
              current_database() AS default_collate_catalog, \
              'pg_catalog' AS default_collate_schema, 'ucs_basic' AS default_collate_name",
    },
    Internal {
        schema: INFORMATION_SCHEMA,
        name: "columns",
        written: "information_schema.\"columns\"",
        sql: "SELECT database_name AS table_catalog, schema_name AS table_schema, table_name, \
              column_name, column_index AS ordinal_position, column_default, \
              CASE  WHEN (is_nullable) THEN ('YES') ELSE 'NO' END AS is_nullable, data_type, \
              character_maximum_length, CAST(NULL AS INTEGER) AS character_octet_length, \
              numeric_precision, numeric_precision_radix, numeric_scale, \
              CAST(NULL AS INTEGER) AS datetime_precision, CAST(NULL AS VARCHAR) AS interval_type, \
              CAST(NULL AS INTEGER) AS interval_precision, \
              CAST(NULL AS VARCHAR) AS character_set_catalog, \
              CAST(NULL AS VARCHAR) AS character_set_schema, \
              CAST(NULL AS VARCHAR) AS character_set_name, \
              CAST(NULL AS VARCHAR) AS collation_catalog, \
              CAST(NULL AS VARCHAR) AS collation_schema, CAST(NULL AS VARCHAR) AS collation_name, \
              CAST(NULL AS VARCHAR) AS domain_catalog, CAST(NULL AS VARCHAR) AS domain_schema, \
              CAST(NULL AS VARCHAR) AS domain_name, CAST(NULL AS VARCHAR) AS udt_catalog, \
              CAST(NULL AS VARCHAR) AS udt_schema, CAST(NULL AS VARCHAR) AS udt_name, \
              CAST(NULL AS VARCHAR) AS scope_catalog, CAST(NULL AS VARCHAR) AS scope_schema, \
              CAST(NULL AS VARCHAR) AS scope_name, CAST(NULL AS BIGINT) AS maximum_cardinality, \
              CAST(NULL AS VARCHAR) AS dtd_identifier, CAST(NULL AS BOOL) AS is_self_referencing, \
              CAST(NULL AS BOOL) AS is_identity, CAST(NULL AS VARCHAR) AS identity_generation, \
              CAST(NULL AS VARCHAR) AS identity_start, CAST(NULL AS VARCHAR) AS identity_increment, \
              CAST(NULL AS VARCHAR) AS identity_maximum, CAST(NULL AS VARCHAR) AS identity_minimum, \
              CAST(NULL AS BOOL) AS identity_cycle, \
              CASE  WHEN (is_generated) THEN ('ALWAYS') ELSE 'NEVER' END AS is_generated, \
              generation_expression, CAST(NULL AS BOOL) AS is_updatable, \
              \"comment\" AS COLUMN_COMMENT FROM duckdb_columns",
    },
    Internal {
        schema: INFORMATION_SCHEMA,
        name: "schemata",
        written: "information_schema.schemata",
        sql: "SELECT database_name AS catalog_name, schema_name, 'duckdb' AS schema_owner, \
              CAST(NULL AS VARCHAR) AS default_character_set_catalog, \
              CAST(NULL AS VARCHAR) AS default_character_set_schema, \
              CAST(NULL AS VARCHAR) AS default_character_set_name, \"sql\" AS sql_path \
              FROM duckdb_schemas()",
    },
    Internal {
        schema: INFORMATION_SCHEMA,
        name: "tables",
        written: "information_schema.\"tables\"",
        sql: "(SELECT database_name AS table_catalog, schema_name AS table_schema, table_name, \
              CASE  WHEN (\"temporary\") THEN ('LOCAL TEMPORARY') ELSE 'BASE TABLE' END \
              AS table_type, CAST(NULL AS VARCHAR) AS self_referencing_column_name, \
              CAST(NULL AS VARCHAR) AS reference_generation, \
              CAST(NULL AS VARCHAR) AS user_defined_type_catalog, \
              CAST(NULL AS VARCHAR) AS user_defined_type_schema, \
              CAST(NULL AS VARCHAR) AS user_defined_type_name, 'YES' AS is_insertable_into, \
              'NO' AS is_typed, CASE  WHEN (\"temporary\") THEN ('PRESERVE') ELSE NULL END \
              AS commit_action, \"comment\" AS TABLE_COMMENT FROM duckdb_tables()) UNION ALL \
              (SELECT database_name AS table_catalog, schema_name AS table_schema, \
              view_name AS table_name, 'VIEW' AS table_type, NULL AS self_referencing_column_name, \
              NULL AS reference_generation, NULL AS user_defined_type_catalog, \
              NULL AS user_defined_type_schema, NULL AS user_defined_type_name, \
              'NO' AS is_insertable_into, 'NO' AS is_typed, NULL AS commit_action, \
              \"comment\" AS TABLE_COMMENT FROM duckdb_views)",
    },
    Internal {
        schema: INFORMATION_SCHEMA,
        name: "views",
        written: "information_schema.\"views\"",
        sql: "SELECT database_name AS table_catalog, schema_name AS table_schema, \
              view_name AS table_name, \"sql\" AS view_definition, 'NONE' AS check_option, \
              'NO' AS is_updatable, 'NO' AS is_insertable_into, 'NO' AS is_trigger_updatable, \
              'NO' AS is_trigger_deletable, 'NO' AS is_trigger_insertable_into FROM duckdb_views()",
    },
];

#[cfg(test)]
mod tests {
    use super::{INTERNAL_VIEWS, statement};

    /// The text the pin prints for one of these, character for character.
    #[test]
    fn a_statement_is_written_the_way_upstream_writes_it() {
        let found = INTERNAL_VIEWS
            .iter()
            .find(|view| view.name == "views")
            .expect("the standard view of views");
        assert_eq!(
            statement(found),
            "CREATE TEMP VIEW information_schema.\"views\" AS SELECT database_name AS \
             table_catalog, schema_name AS table_schema, view_name AS table_name, \"sql\" AS \
             view_definition, 'NONE' AS check_option, 'NO' AS is_updatable, 'NO' AS \
             is_insertable_into, 'NO' AS is_trigger_updatable, 'NO' AS is_trigger_deletable, 'NO' \
             AS is_trigger_insertable_into FROM duckdb_views();"
        );
    }

    /// A name that is written twice over is a name that can be written two different ways.
    #[test]
    fn every_written_name_ends_in_the_name_the_view_answers_to() {
        for view in INTERNAL_VIEWS {
            let bare = view.written.trim_end_matches('"');
            assert!(
                bare.ends_with(view.name),
                "{} is written as {}, which does not end in the name",
                view.name,
                view.written
            );
        }
    }
}
