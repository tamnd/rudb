//! The scalar, aggregate and window function library.
//!
//! Rank 7 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Most of what is here is the signature half: given a name and the types of the arguments, which
//! function is that and what does it return. The implementations are separate work and they go
//! behind the same names, so the binder does not change when they arrive.
//!
//! [`mod@file`] is the exception and it reads a file. A table function that scans a Parquet file cannot
//! be resolved from a table of names, because its columns are in the file, so resolving one means
//! opening it. That is the only I/O in this crate and it is the reason `rudb-io` and `rudb-parquet`
//! are dependencies of it.

#![forbid(unsafe_code)]

pub mod entrycatalog;
pub mod file;
pub mod functioncatalog;
pub mod settingcatalog;
pub mod signature;
pub mod table;
pub mod typecatalog;

pub use entrycatalog::{
    DUCKDB, canonical, column_fields, database_fields, numeric_facts, schema_fields, table_fields,
    view_fields,
};
pub use file::{
    csv_fields, csv_given, files, is_file, is_pattern, open_csv, open_parquet, parquet_fields,
};
pub use functioncatalog::{
    CONSISTENT, FUNCTION_CATALOG, FUNCTION_SCHEMA, FunctionEntry, function_entries, function_fields,
};
pub use rudb_csv::Given;
pub use settingcatalog::{
    GLOBAL, SETTINGS, SettingEntry, setting_fields, setting_named, unknown_setting,
};
pub use signature::{
    FunctionKind, FunctionRow, Resolved, function_rows, kind_of, part_type, resolve,
};
pub use table::{
    Columns, FILE_ROW_NUMBER, ResolvedTable, TableFunction, extension_fields, keyword_categories,
    keyword_fields, optimizer_fields, resolve_table, series, series_length, strategy_fields,
};
pub use typecatalog::{
    Signature, TYPE_NAMES, TypeEntry, representative, sort_key, type_category, type_fields,
    type_oid, type_size,
};
