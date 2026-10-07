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
//! Thirty six of the 47: the wrappers in `main` but `duckdb_logs`, five of the standard views, and
//! all twenty two of `pg_catalog`, two of which read a table function rudb does not have yet and
//! say so when they are read. The other eleven are `duckdb_logs`, the four `sqlite_` views and the
//! six `information_schema` constraint views, which are the ones a client is most likely to miss.
//! Nothing here is a rudb invention: a view is either upstream's own text or it is absent, so a
//! client that finds one can trust it answers the way the pin does.

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
        name: "duckdb_constraints",
        written: "duckdb_constraints",
        sql: "SELECT * FROM duckdb_constraints()",
    },
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
        name: "duckdb_indexes",
        written: "duckdb_indexes",
        sql: "SELECT * FROM duckdb_indexes()",
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
    // The PostgreSQL views, in the pin's order and with its bodies. Two of them read a table
    // function that is not here yet, `duckdb_dependencies` and `duckdb_prepared_statements`, and
    // say so when they are read.
    Internal {
        schema: PG_CATALOG,
        name: "pg_am",
        written: "pg_catalog.pg_am",
        sql: "SELECT 0 AS oid, 'art' AS amname, NULL AS amhandler, 'i' AS amtype",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_prepared_statements",
        written: "pg_catalog.pg_prepared_statements",
        sql: "SELECT \"name\", \"statement\", NULL AS prepare_time, parameter_types, \
              result_types, NULL AS from_sql, NULL AS generic_plans, NULL AS custom_plans FROM \
              duckdb_prepared_statements()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_attribute",
        written: "pg_catalog.pg_attribute",
        sql: "SELECT table_oid AS attrelid, column_name AS attname, data_type_id AS \
              atttypid, 0 AS attstattarget, NULL AS attlen, column_index AS attnum, 0 AS \
              attndims, -1 AS attcacheoff, CASE  WHEN ((data_type ~~* '%decimal%')) THEN \
              (((numeric_precision * 1000) + numeric_scale)) ELSE -1 END AS atttypmod, false AS \
              attbyval, NULL AS attstorage, NULL AS attalign, (NOT is_nullable) AS attnotnull, \
              (column_default IS NOT NULL) AS atthasdef, false AS atthasmissing, '' AS \
              attidentity, '' AS attgenerated, false AS attisdropped, true AS attislocal, 0 AS \
              attinhcount, 0 AS attcollation, NULL AS attcompression, NULL AS attacl, NULL AS \
              attoptions, NULL AS attfdwoptions, NULL AS attmissingval FROM duckdb_columns()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_attrdef",
        written: "pg_catalog.pg_attrdef",
        sql: "SELECT column_index AS oid, table_oid AS adrelid, column_index AS adnum, \
              column_default AS adbin FROM duckdb_columns() WHERE (column_default IS NOT NULL)",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_class",
        written: "pg_catalog.pg_class",
        sql: "(SELECT table_oid AS oid, table_name AS relname, schema_oid AS relnamespace, \
              0 AS reltype, 0 AS reloftype, 0 AS relowner, 0 AS relam, 0 AS relfilenode, 0 AS \
              reltablespace, 0 AS relpages, CAST(estimated_size AS FLOAT) AS reltuples, 0 AS \
              relallvisible, 0 AS reltoastrelid, 0 AS reltoastidxid, (index_count > 0) AS \
              relhasindex, false AS relisshared, CASE  WHEN (\"temporary\") THEN ('t') ELSE 'p' \
              END AS relpersistence, 'r' AS relkind, column_count AS relnatts, \
              check_constraint_count AS relchecks, false AS relhasoids, has_primary_key AS \
              relhaspkey, false AS relhasrules, false AS relhastriggers, false AS relhassubclass, \
              false AS relrowsecurity, true AS relispopulated, NULL AS relreplident, false AS \
              relispartition, 0 AS relrewrite, 0 AS relfrozenxid, NULL AS relminmxid, NULL AS \
              relacl, NULL AS reloptions, NULL AS relpartbound FROM duckdb_tables()) UNION ALL \
              (SELECT view_oid AS oid, view_name AS relname, schema_oid AS relnamespace, 0 AS \
              reltype, 0 AS reloftype, 0 AS relowner, 0 AS relam, 0 AS relfilenode, 0 AS \
              reltablespace, 0 AS relpages, 0 AS reltuples, 0 AS relallvisible, 0 AS \
              reltoastrelid, 0 AS reltoastidxid, false AS relhasindex, false AS relisshared, \
              CASE  WHEN (\"temporary\") THEN ('t') ELSE 'p' END AS relpersistence, 'v' AS \
              relkind, column_count AS relnatts, 0 AS relchecks, false AS relhasoids, false AS \
              relhaspkey, false AS relhasrules, false AS relhastriggers, false AS relhassubclass, \
              false AS relrowsecurity, true AS relispopulated, NULL AS relreplident, false AS \
              relispartition, 0 AS relrewrite, 0 AS relfrozenxid, NULL AS relminmxid, NULL AS \
              relacl, NULL AS reloptions, NULL AS relpartbound FROM duckdb_views())UNION ALL \
              (SELECT sequence_oid AS oid, sequence_name AS relname, schema_oid AS relnamespace, \
              0 AS reltype, 0 AS reloftype, 0 AS relowner, 0 AS relam, 0 AS relfilenode, 0 AS \
              reltablespace, 0 AS relpages, 0 AS reltuples, 0 AS relallvisible, 0 AS \
              reltoastrelid, 0 AS reltoastidxid, false AS relhasindex, false AS relisshared, \
              CASE  WHEN (\"temporary\") THEN ('t') ELSE 'p' END AS relpersistence, 'S' AS \
              relkind, 0 AS relnatts, 0 AS relchecks, false AS relhasoids, false AS relhaspkey, \
              false AS relhasrules, false AS relhastriggers, false AS relhassubclass, false AS \
              relrowsecurity, true AS relispopulated, NULL AS relreplident, false AS \
              relispartition, 0 AS relrewrite, 0 AS relfrozenxid, NULL AS relminmxid, NULL AS \
              relacl, NULL AS reloptions, NULL AS relpartbound FROM duckdb_sequences())UNION ALL \
              (SELECT index_oid AS oid, index_name AS relname, schema_oid AS relnamespace, 0 AS \
              reltype, 0 AS reloftype, 0 AS relowner, 0 AS relam, 0 AS relfilenode, 0 AS \
              reltablespace, 0 AS relpages, 0 AS reltuples, 0 AS relallvisible, 0 AS \
              reltoastrelid, 0 AS reltoastidxid, false AS relhasindex, false AS relisshared, 't' \
              AS relpersistence, 'i' AS relkind, NULL AS relnatts, 0 AS relchecks, false AS \
              relhasoids, false AS relhaspkey, false AS relhasrules, false AS relhastriggers, \
              false AS relhassubclass, false AS relrowsecurity, true AS relispopulated, NULL AS \
              relreplident, false AS relispartition, 0 AS relrewrite, 0 AS relfrozenxid, NULL AS \
              relminmxid, NULL AS relacl, NULL AS reloptions, NULL AS relpartbound FROM \
              duckdb_indexes())",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_constraint",
        written: "pg_catalog.pg_constraint",
        sql: "SELECT ((table_oid * 1000000) + constraint_index) AS oid, constraint_text AS \
              conname, schema_oid AS connamespace, CASE  WHEN ((constraint_type = 'CHECK')) THEN \
              ('c') WHEN ((constraint_type = 'UNIQUE')) THEN ('u') WHEN ((constraint_type = \
              'PRIMARY KEY')) THEN ('p') WHEN ((constraint_type = 'FOREIGN KEY')) THEN ('f') ELSE \
              'x' END AS contype, false AS condeferrable, false AS condeferred, true AS \
              convalidated, table_oid AS conrelid, 0 AS contypid, 0 AS conindid, 0 AS \
              conparentid, 0 AS confrelid, NULL AS confupdtype, NULL AS confdeltype, NULL AS \
              confmatchtype, true AS conislocal, 0 AS coninhcount, false AS connoinherit, \
              constraint_column_indexes AS conkey, NULL AS confkey, NULL AS conpfeqop, NULL AS \
              conppeqop, NULL AS conffeqop, NULL AS conexclop, expression AS conbin FROM \
              duckdb_constraints()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_collation",
        written: "pg_catalog.pg_collation",
        sql: "SELECT CAST(NULL AS OID) AS oid, CAST(NULL AS VARCHAR) AS collname WHERE \
              false",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_database",
        written: "pg_catalog.pg_database",
        sql: "SELECT database_oid AS oid, database_name AS datname, CAST(true AS BOOLEAN) \
              AS datallowconn, CAST(false AS BOOLEAN) AS datistemplate FROM duckdb_databases()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_depend",
        written: "pg_catalog.pg_depend",
        sql: "SELECT * FROM duckdb_dependencies()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_description",
        written: "pg_catalog.pg_description",
        sql: "(SELECT table_oid AS objoid, database_oid AS classoid, 0 AS objsubid, \
              \"comment\" AS description FROM duckdb_tables() WHERE (NOT internal)) UNION ALL \
              (SELECT table_oid AS objoid, database_oid AS classoid, column_index AS objsubid, \
              \"comment\" AS description FROM duckdb_columns() WHERE (NOT internal))UNION ALL \
              (SELECT view_oid AS objoid, database_oid AS classoid, 0 AS objsubid, \"comment\" AS \
              description FROM duckdb_views() WHERE (NOT internal))UNION ALL (SELECT index_oid AS \
              objoid, database_oid AS classoid, 0 AS objsubid, \"comment\" AS description FROM \
              duckdb_indexes)UNION ALL (SELECT sequence_oid AS objoid, database_oid AS classoid, \
              0 AS objsubid, \"comment\" AS description FROM duckdb_sequences())UNION ALL (SELECT \
              type_oid AS objoid, database_oid AS classoid, 0 AS objsubid, \"comment\" AS \
              description FROM duckdb_types() WHERE (NOT internal))UNION ALL (SELECT function_oid \
              AS objoid, database_oid AS classoid, 0 AS objsubid, \"comment\" AS description FROM \
              duckdb_functions() WHERE (NOT internal))",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_enum",
        written: "pg_catalog.pg_enum",
        sql: "SELECT NULL AS oid, a.type_oid AS enumtypid, list_position(b.labels, \
              a.elabel) AS enumsortorder, a.elabel AS enumlabel FROM ((SELECT unnest(labels) AS \
              elabel, type_oid FROM duckdb_types() WHERE (logical_type = 'ENUM')) AS a INNER JOIN \
              duckdb_types() AS b ON ((a.type_oid = b.type_oid)))",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_index",
        written: "pg_catalog.pg_index",
        sql: "SELECT index_oid AS indexrelid, table_oid AS indrelid, 0 AS indnatts, 0 AS \
              indnkeyatts, is_unique AS indisunique, is_primary AS indisprimary, false AS \
              indisexclusion, true AS indimmediate, false AS indisclustered, true AS indisvalid, \
              false AS indcheckxmin, true AS indisready, true AS indislive, false AS \
              indisreplident, CAST(NULL AS INTEGER[]) AS indkey, CAST(NULL AS OID[]) AS \
              indcollation, CAST(NULL AS OID[]) AS indclass, CAST(NULL AS INTEGER[]) AS \
              indoption, expressions AS indexprs, NULL AS indpred FROM duckdb_indexes()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_indexes",
        written: "pg_catalog.pg_indexes",
        sql: "SELECT schema_name AS schemaname, table_name AS tablename, index_name AS \
              indexname, NULL AS \"tablespace\", \"sql\" AS indexdef FROM duckdb_indexes()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_namespace",
        written: "pg_catalog.pg_namespace",
        sql: "SELECT oid, schema_name AS nspname, 0 AS nspowner, NULL AS nspacl FROM \
              duckdb_schemas() WHERE (database_name = current_database())",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_proc",
        written: "pg_catalog.pg_proc",
        sql: "SELECT f.function_oid AS oid, function_name AS proname, s.oid AS \
              pronamespace, NULL AS proowner, NULL AS prolang, 0 AS procost, 0 AS prorows, \
              varargs AS provariadic, 0 AS prosupport, CASE  WHEN ((function_type = 'aggregate')) \
              THEN ('a') ELSE 'f' END AS prokind, false AS prosecdef, false AS proleakproof, \
              false AS proisstrict, (function_type = 'table') AS proretset, CASE  WHEN \
              ((stability = 'CONSISTENT')) THEN ('i') WHEN ((stability = \
              'CONSISTENT_WITHIN_QUERY')) THEN ('s') WHEN ((stability = 'VOLATILE')) THEN ('v') \
              ELSE NULL END AS provolatile, 'u' AS proparallel, length(parameters) AS pronargs, 0 \
              AS pronargdefaults, return_type AS prorettype, parameter_types AS proargtypes, NULL \
              AS proallargtypes, NULL AS proargmodes, parameters AS proargnames, NULL AS \
              proargdefaults, NULL AS protrftypes, NULL AS prosrc, NULL AS probin, \
              macro_definition AS prosqlbody, NULL AS proconfig, NULL AS proacl, (function_type = \
              'aggregate') AS proisagg FROM (duckdb_functions() AS f LEFT JOIN duckdb_schemas() \
              AS s USING (database_name, schema_name))",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_sequence",
        written: "pg_catalog.pg_sequence",
        sql: "SELECT sequence_oid AS seqrelid, 0 AS seqtypid, start_value AS seqstart, \
              increment_by AS seqincrement, max_value AS seqmax, min_value AS seqmin, 0 AS \
              seqcache, \"cycle\" AS seqcycle FROM duckdb_sequences()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_sequences",
        written: "pg_catalog.pg_sequences",
        sql: "SELECT schema_name AS schemaname, sequence_name AS sequencename, 'duckdb' AS \
              sequenceowner, 0 AS data_type, start_value, min_value, max_value, increment_by, \
              \"cycle\", 0 AS cache_size, last_value FROM duckdb_sequences()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_settings",
        written: "pg_catalog.pg_settings",
        sql: "SELECT \"name\", \"value\" AS setting, description AS short_desc, CASE  WHEN \
              ((input_type = 'VARCHAR')) THEN ('string') WHEN ((input_type = 'BOOLEAN')) THEN \
              ('bool') WHEN ((input_type IN ('BIGINT', 'UBIGINT'))) THEN ('integer') ELSE \
              input_type END AS vartype FROM duckdb_settings()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_tables",
        written: "pg_catalog.pg_tables",
        sql: "SELECT schema_name AS schemaname, table_name AS tablename, 'duckdb' AS \
              tableowner, NULL AS \"tablespace\", (index_count > 0) AS hasindexes, false AS \
              hasrules, false AS hastriggers FROM duckdb_tables()",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_tablespace",
        written: "pg_catalog.pg_tablespace",
        sql: "SELECT 0 AS oid, 'pg_default' AS spcname, 0 AS spcowner, NULL AS spcacl, NULL \
              AS spcoptions",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_type",
        written: "pg_catalog.pg_type",
        sql: "SELECT CASE  WHEN ((type_oid IS NULL)) THEN (NULL) WHEN (((logical_type = \
              'ENUM') AND (type_name != 'enum'))) THEN (type_oid) ELSE map_to_pg_oid(type_name) \
              END AS oid, format_pg_type(logical_type, type_name) AS typname, schema_oid AS \
              typnamespace, 0 AS typowner, type_size AS typlen, false AS typbyval, CASE  WHEN \
              ((logical_type = 'ENUM')) THEN ('e') ELSE 'b' END AS typtype, CASE  WHEN \
              ((type_category = 'NUMERIC')) THEN ('N') WHEN ((type_category = 'STRING')) THEN \
              ('S') WHEN ((type_category = 'DATETIME')) THEN ('D') WHEN ((type_category = \
              'BOOLEAN')) THEN ('B') WHEN ((type_category = 'COMPOSITE')) THEN ('C') WHEN \
              ((type_category = 'USER')) THEN ('U') ELSE 'X' END AS typcategory, false AS \
              typispreferred, true AS typisdefined, NULL AS typdelim, NULL AS typrelid, NULL AS \
              typsubscript, NULL AS typelem, NULL AS typarray, NULL AS typinput, NULL AS \
              typoutput, NULL AS typreceive, NULL AS typsend, NULL AS typmodin, NULL AS \
              typmodout, NULL AS typanalyze, 'd' AS typalign, 'p' AS typstorage, NULL AS \
              typnotnull, NULL AS typbasetype, NULL AS typtypmod, NULL AS typndims, NULL AS \
              typcollation, NULL AS typdefaultbin, NULL AS typdefault, NULL AS typacl FROM \
              duckdb_types() WHERE (type_oid IS NOT NULL)",
    },
    Internal {
        schema: PG_CATALOG,
        name: "pg_views",
        written: "pg_catalog.pg_views",
        sql: "SELECT schema_name AS schemaname, view_name AS viewname, 'duckdb' AS \
              viewowner, \"sql\" AS definition FROM duckdb_views()",
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
