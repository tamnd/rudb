//! The configuration knobs, what each one means here, and which ones are refused.
//!
//! DuckDB has around a hundred and sixty of these and its own test corpus sets them constantly, at
//! the top of a file to make a plan deterministic or in the middle of one to force a code path.
//! There are 1336 `SET` records and 800 `PRAGMA` records in the corpus, and a `SET` that fails at
//! the top of a file usually takes the rest of the file with it, so the count of records this
//! reaches is larger than the count of records it is.
//!
//! The temptation is to accept every one of them and ignore all of them, which would turn two
//! thousand failures into two thousand passes this afternoon and would make the number a lie. So
//! each name is classified, and the rule for which side of the line a name falls on is one
//! sentence:
//!
//! **A setting is refused when ignoring it would change which rows a query gives back.**
//!
//! `SET default_null_order='nulls_first'` changes which rows come out of `ORDER BY`, so it is
//! either honoured or refused, never remembered. `SET threads=4` cannot change an answer on any
//! engine that is not broken, so it is remembered, reported back, and otherwise does nothing.
//!
//! Two cases are worth arguing out loud because they look like they fall the other way.
//!
//! `memory_limit` and `max_execution_time` are remembered rather than refused, and rudb enforces
//! neither. A limit that is not enforced means a query DuckDB stops with an out of memory error
//! succeeds here. That is a difference in whether a query errors and not in which rows it gives
//! back, the memory manager is M6 work in `spec/17-milestones.md`, and refusing the setting would
//! not make rudb any more likely to run out of memory in the same place DuckDB does.
//!
//! `preserve_insertion_order` is remembered although it plainly affects order, because it only ever
//! gets turned off. It is on by default and it is a promise, so a file that turns it off is
//! releasing rudb from a promise rather than asking for something new, and keeping the promise
//! anyway is a legal answer to the relaxed question. If it ever defaults to off upstream that stops
//! being true and the name moves to the refused list.
//!
//! A name that is in none of the tables is an error, with DuckDB's own wording, because
//! `SET some_typo=1` succeeding quietly is worse than any of the above.
//!
//! The tables were read off `duckdb_settings()` and `duckdb_functions()` on a real binary rather
//! than typed out, then extended with the names the v2.0 corpus uses that the newest released
//! binary does not have yet. That second half is the part that goes stale, and it goes stale in the
//! safe direction: a v2.0 setting nobody has added here is an error rather than a silent accept.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::error::{Error, Result};
use crate::value::Value;

/// What rudb does with a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// rudb implements it, and reading it back says what rudb is doing.
    Honoured,
    /// It cannot change which rows come back here, so it is stored and otherwise ignored.
    Remembered,
    /// It would change which rows come back and rudb cannot honour it yet, so setting it is an
    /// error rather than a quiet wrong answer.
    Refused,
}

/// Which of `SET`, `SET GLOBAL`, `SET SESSION` and `SET LOCAL` a statement was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// No scope word, which means whichever one the setting lives in.
    #[default]
    Unstated,
    /// `GLOBAL`, which is where all but sixteen of them live.
    Global,
    /// `SESSION`, which DuckDB spells local internally and which most settings refuse.
    Session,
    /// `LOCAL`, which DuckDB parses and then declines to implement.
    Local,
}

/// Where the nulls go when the query does not say.
///
/// Four values rather than two, because two of DuckDB's four make the answer depend on the
/// direction. The default is `NULLS_LAST` for both directions, which is worth pinning down because
/// it is not the rule most people assume: `ORDER BY x DESC` is not the exact reverse of
/// `ORDER BY x` in DuckDB, the nulls stay at the bottom of both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullOrder {
    /// Nulls before values, both directions.
    First,
    /// Nulls after values, both directions. DuckDB's default.
    #[default]
    Last,
    /// Nulls first ascending, last descending. What `sqlite` and `mysql` mean.
    FirstOnAscending,
    /// Nulls last ascending, first descending. What `postgres` means.
    LastOnAscending,
}

impl NullOrder {
    /// Whether nulls come first, given the direction the key is sorted in.
    #[must_use]
    pub const fn first_when(self, descending: bool) -> bool {
        match self {
            Self::First => true,
            Self::Last => false,
            Self::FirstOnAscending => !descending,
            Self::LastOnAscending => descending,
        }
    }

    /// The name DuckDB reports this under, which is what `current_setting` gives back.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::First => "NULLS_FIRST",
            Self::Last => "NULLS_LAST",
            Self::FirstOnAscending => "NULLS_FIRST_ON_ASC_LAST_ON_DESC",
            Self::LastOnAscending => "NULLS_LAST_ON_ASC_FIRST_ON_DESC",
        }
    }

    /// Read one of the spellings DuckDB takes.
    ///
    /// The three database names are in here because DuckDB takes them, and each one means the rule
    /// that database uses. `sqlite` and `mysql` are the same rule and `postgres` is the other one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let flat: String = text.chars().filter(|c| !c.is_whitespace() && *c != '_').collect();
        match flat.to_ascii_lowercase().as_str() {
            "nullsfirst" => Some(Self::First),
            "nullslast" => Some(Self::Last),
            "nullsfirstonasclastondesc" | "sqlite" | "mysql" => Some(Self::FirstOnAscending),
            "nullslastonascfirstondesc" | "postgres" => Some(Self::LastOnAscending),
            _ => None,
        }
    }
}

impl fmt::Display for NullOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which way a sort key goes when the query does not say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultOrder {
    /// Smallest first.
    #[default]
    Ascending,
    /// Largest first.
    Descending,
}

impl DefaultOrder {
    /// Whether a key with no direction written on it sorts downwards.
    #[must_use]
    pub const fn descending(self) -> bool {
        matches!(self, Self::Descending)
    }

    /// The name DuckDB reports this under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ascending => "ASCENDING",
            Self::Descending => "DESCENDING",
        }
    }

    /// Read one of the spellings DuckDB takes.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "asc" | "ascending" => Some(Self::Ascending),
            "desc" | "descending" => Some(Self::Descending),
            _ => None,
        }
    }
}

impl fmt::Display for DefaultOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The settings a database is running under.
///
/// The two that are honoured are held as themselves rather than as strings in the map, because the
/// binder reads them once per sort key and a string comparison per key is a string comparison that
/// can be spelled wrong. Everything remembered is a value in the map, since nothing reads it.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    null_order: NullOrder,
    order: DefaultOrder,
    remembered: BTreeMap<String, Value>,
    toggled: BTreeSet<String>,
}

impl Settings {
    /// The defaults, which are DuckDB's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the nulls go when the query does not say.
    #[must_use]
    pub const fn null_order(&self) -> NullOrder {
        self.null_order
    }

    /// Which way a sort key goes when the query does not say.
    #[must_use]
    pub const fn order(&self) -> DefaultOrder {
        self.order
    }

    /// Whether a `PRAGMA` switch of this name has been thrown.
    ///
    /// Nothing reads this yet. It exists so the switches are recorded rather than dropped, and so
    /// that the day one of them starts meaning something there is somewhere to read it from.
    #[must_use]
    pub fn toggled(&self, name: &str) -> bool {
        self.toggled.contains(&name.to_ascii_lowercase())
    }

    /// Apply `SET name = value`, in the scope the statement asked for.
    ///
    /// # Errors
    ///
    /// When the name is not a setting, when it is one rudb refuses, when the scope is one the
    /// setting does not live in, or when the value is not one that setting takes.
    pub fn set(&mut self, name: &str, scope: Scope, value: &Value) -> Result<()> {
        let support = support_of(name)?;
        let lower = canonical(name);
        match scope {
            Scope::Local => return Err(Error::not_implemented("SET LOCAL is not implemented.")),
            Scope::Session if !SESSION_SCOPE.contains(&lower.as_str()) => {
                return Err(Error::catalog(format!("option \"{name}\" cannot be set locally")));
            }
            _ => {}
        }
        match support {
            Support::Refused => Err(refused(&lower)),
            Support::Remembered => {
                self.remembered.insert(lower, value.clone());
                Ok(())
            }
            Support::Honoured => self.honour(&lower, value),
        }
    }

    /// Apply `RESET name`, putting it back to what it was before anybody set it.
    ///
    /// A `RESET` of a refused setting is allowed where a `SET` of it is not. Refusing it would be
    /// refusing to go back to the behaviour rudb already has.
    ///
    /// # Errors
    ///
    /// When the name is not a setting.
    pub fn reset(&mut self, name: &str) -> Result<()> {
        support_of(name)?;
        let lower = canonical(name);
        self.remembered.remove(&lower);
        match lower.as_str() {
            "default_null_order" => self.null_order = NullOrder::default(),
            "default_order" => self.order = DefaultOrder::default(),
            _ => {}
        }
        Ok(())
    }

    /// Apply a `PRAGMA` that is a bare name rather than an assignment, such as
    /// `PRAGMA disable_profiling`.
    ///
    /// These are a separate namespace from the settings, which is not obvious and is easy to get
    /// wrong in both directions: `PRAGMA disable_profiling` works and `SET disable_profiling=1`
    /// does not, `SET threads=4` works and `PRAGMA threads` does not.
    ///
    /// # Errors
    ///
    /// When the name is not one of them, or is one that produces rows.
    pub fn toggle(&mut self, name: &str) -> Result<()> {
        let lower = name.to_ascii_lowercase();
        if PRAGMA_TOGGLES.contains(&lower.as_str()) {
            self.toggled.insert(lower);
            return Ok(());
        }
        if PRAGMA_TABLES.contains(&lower.as_str()) {
            return Err(Error::not_implemented(format!(
                "PRAGMA {lower} produces rows and rudb does not implement it yet"
            )));
        }
        Err(Error::catalog(format!("Pragma Function with name {name} does not exist!")))
    }

    /// What `current_setting(name)` gives back.
    ///
    /// # Errors
    ///
    /// When the name is not a setting.
    pub fn get(&self, name: &str) -> Result<Value> {
        support_of(name)?;
        let lower = canonical(name);
        match lower.as_str() {
            "default_null_order" => return Ok(Value::Varchar(self.null_order.name().to_owned())),
            "default_order" => return Ok(Value::Varchar(self.order.name().to_owned())),
            _ => {}
        }
        Ok(self.remembered.get(&lower).cloned().unwrap_or(Value::Null))
    }

    /// The honoured half of [`Settings::set`], where the value has to be one of a fixed few.
    fn honour(&mut self, lower: &str, value: &Value) -> Result<()> {
        let text = match value {
            Value::Varchar(text) => text.clone(),
            other => other.to_string(),
        };
        match lower {
            "default_null_order" => {
                let Some(order) = NullOrder::parse(&text) else {
                    return Err(Error::parser(format!(
                        "Unrecognized parameter for option NULL_ORDER \"{text}\", expected either NULLS FIRST, NULLS LAST, SQLite, MySQL or Postgres"
                    )));
                };
                self.null_order = order;
                Ok(())
            }
            "default_order" => {
                let Some(order) = DefaultOrder::parse(&text) else {
                    return Err(Error::invalid_input(format!(
                        "Unrecognized parameter for option DEFAULT_ORDER \"{text}\". Expected ASC or DESC."
                    )));
                };
                self.order = order;
                Ok(())
            }
            _ => Err(Error::internal(format!("{lower} is marked honoured and is not handled"))),
        }
    }
}

/// The name a setting is stored under, which is DuckDB's own name for it, lowercased.
///
/// Six of them have a second spelling, and the pair is one setting rather than two of them, so
/// `SET memory_limit='1GB'` followed by `SELECT current_setting('max_memory')` has to answer. The
/// direction of each fold is DuckDB's, read off the `aliases` column of `duckdb_settings()`, which
/// is why `memory_limit` folds onto `max_memory` and not the other way about.
fn canonical(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "null_order" => "default_null_order".to_owned(),
        "memory_limit" => "max_memory".to_owned(),
        "wal_autocheckpoint" => "checkpoint_threshold".to_owned(),
        "profiling_output" => "profile_output".to_owned(),
        "worker_threads" => "threads".to_owned(),
        "user" => "username".to_owned(),
        _ => lower,
    }
}

/// What rudb does with a name, or the error for a name that is not a setting.
///
/// # Errors
///
/// When the name is not a setting, with DuckDB's own wording. DuckDB follows that line with a
/// suggestion of the nearest name it does know, which is not reproduced here, because a suggestion
/// drawn from a slightly different table of names is worse than no suggestion.
pub fn support_of(name: &str) -> Result<Support> {
    let lower = canonical(name);
    if REFUSED.contains(&lower.as_str()) {
        return Ok(Support::Refused);
    }
    if HONOURED.contains(&lower.as_str()) {
        return Ok(Support::Honoured);
    }
    if KNOWN.contains(&lower.as_str()) {
        return Ok(Support::Remembered);
    }
    Err(Error::catalog(format!("unrecognized configuration parameter \"{name}\"")))
}

/// The error for a setting rudb will not accept.
fn refused(lower: &str) -> Error {
    Error::not_implemented(format!(
        "{lower} changes which rows a query gives back and rudb does not implement it yet, so it is refused rather than ignored"
    ))
}

/// The settings rudb implements.
const HONOURED: &[&str] = &["default_null_order", "default_order"];

/// The settings rudb refuses, because ignoring one would change which rows come back.
///
/// Each of these is a feature that does not exist yet rather than a decision not to have it, and
/// the day the feature lands the name moves out of this list. `timezone` and `calendar` need a
/// timestamp with time zone that respects one. `schema` and `search_path` need catalog resolution
/// to read them. `integer_division`, `old_implicit_casting`, `null_on_division_by_zero` and
/// `ieee_floating_point_ops` each change what an arithmetic expression evaluates to.
/// `binary_as_string` and `sqlite_all_varchar` change the type a column comes back as, which
/// changes the values in it. The four `arrow_` names change the buffers a result is handed over in,
/// which is a row difference to anybody reading it through Arrow.
const REFUSED: &[&str] = &[
    "arrow_large_buffer_size",
    "arrow_lossless_conversion",
    "arrow_output_list_view",
    "arrow_output_version",
    "binary_as_string",
    "calendar",
    "current_dialect",
    "default_collation",
    "deprecated_using_key_syntax",
    "dialect_compatibility_mode",
    "disable_timestamptz_casts",
    "errors_as_json",
    "file_search_path",
    "ieee_floating_point_ops",
    "integer_division",
    "json_geometry_format",
    "lambda_syntax",
    "legacy_disable_null_type",
    "null_on_division_by_zero",
    "old_implicit_casting",
    "order_by_non_integer_literal",
    "preserve_identifier_case",
    "produce_arrow_string_view",
    "regex_match_operator_semantics",
    "scalar_subquery_error_on_multiple_rows",
    "schema",
    "search_path",
    "show_behavior",
    "sqlite_all_varchar",
    "table_function_identifier_conversion",
    "timezone",
    "warnings_as_errors",
];

/// Every other setting DuckDB has, which rudb stores and ignores.
///
/// Read off `duckdb_settings()` rather than typed out, because the point of the list is that a name
/// DuckDB knows is a name rudb knows, and a name neither of them knows is an error on both. The six
/// aliases are folded in [`canonical`] rather than repeated here.
const KNOWN: &[&str] = &[
    "__delta_only_variant_encoding_enabled",
    "access_mode",
    "allocator_background_threads",
    "allocator_bulk_deallocation_flush_threshold",
    "allocator_flush_threshold",
    "allow_community_extensions",
    "allow_extension_repositories",
    "allow_extensions_metadata_mismatch",
    "allow_parser_override_extension",
    "allow_persistent_secrets",
    "allow_unredacted_secrets",
    "allow_unsigned_extensions",
    "allowed_configs",
    "allowed_directories",
    "allowed_paths",
    "approximate_join_order_threshold",
    "asof_loop_join_threshold",
    "async_threads",
    "auto_checkpoint_skip_wal_threshold",
    "autoinstall_extension_repository",
    "autoinstall_known_extensions",
    "autoload_known_extensions",
    "block_allocator_memory",
    "cache_local_files",
    "catalog_error_max_schemas",
    "checkpoint_on_detach",
    "checkpoint_threshold",
    "configure_metrics",
    "current_transaction_invalidation_policy",
    "custom_extension_repository",
    "custom_profiling_settings",
    "custom_user_agent",
    "debug_asof_iejoin",
    "debug_checkpoint_abort",
    "debug_checkpoint_sleep_ms",
    "debug_eviction_queue_sleep_micro_seconds",
    "debug_force_commit_failure",
    "debug_force_commit_revert_failure",
    "debug_force_external",
    "debug_force_fetch_row",
    "debug_force_no_cross_product",
    "debug_fs_delay_mean_ms",
    "debug_fs_delay_stddev_ms",
    "debug_fs_random_seed",
    "debug_local_file_system_delay_ms",
    "debug_physical_table_scan_execution_strategy",
    "debug_skip_checkpoint_on_commit",
    "debug_verify_blocks",
    "debug_verify_column_bindings",
    "debug_verify_serializer",
    "debug_verify_statement",
    "debug_verify_vector",
    "debug_window_mode",
    "default_block_size",
    "default_secret_storage",
    "default_transaction_invalidation_policy",
    "delim_join_as_cte",
    "disable_database_invalidation",
    "disable_parquet_prefetching",
    "disabled_compression_methods",
    "disabled_filesystems",
    "disabled_log_types",
    "disabled_optimizers",
    "duckdb_api",
    "dynamic_or_filter_threshold",
    "enable_caching_operators",
    "enable_external_access",
    "enable_external_file_cache",
    "enable_fsst_vectors",
    "enable_geoparquet_conversion",
    "enable_http_logging",
    "enable_http_metadata_cache",
    "enable_logging",
    "enable_macro_dependencies",
    "enable_object_cache",
    "enable_optimizer",
    "enable_profiling",
    "enable_progress_bar",
    "enable_progress_bar_print",
    "enable_view_dependencies",
    "enabled_log_types",
    "experimental_metadata_reuse",
    "explain_output",
    "extension_directories",
    "extension_directory",
    "extension_repository_directory",
    "external_file_cache_local_block_size",
    "external_file_cache_remote_block_size",
    "external_threads",
    "force_bitpacking_mode",
    "force_column_metadata_reuse",
    "force_compression",
    "force_mbedtls_unsafe",
    "force_update_to_del_and_insert",
    "force_variant_shredding",
    "geometry_minimum_shredding_size",
    "heap_based_parser",
    "home_directory",
    "http_logging_output",
    "http_proxy",
    "http_proxy_password",
    "http_proxy_username",
    "http_retries",
    "ignore_unknown_crs",
    "immediate_transaction_mode",
    "index_scan_max_count",
    "index_scan_percentage",
    "initial_column_segment_size",
    "late_materialization_max_rows",
    "legacy_metrics_format",
    "lock_configuration",
    "log_query_path",
    "logging_level",
    "logging_mode",
    "logging_storage",
    "max_execution_time",
    "max_expression_depth",
    "max_memory",
    "max_streaming_buffer_size",
    "max_temp_directory_size",
    "max_vacuum_tasks",
    "merge_join_threshold",
    "nested_loop_join_threshold",
    "operator_memory_limit",
    "ordered_aggregate_threshold",
    "parquet_metadata_cache",
    "parquet_prefetch_column_gap",
    "partitioned_write_flush_threshold",
    "partitioned_write_max_open_files",
    "password",
    "perfect_ht_threshold",
    "pin_threads",
    "pivot_filter_threshold",
    "pivot_limit",
    "prefer_range_joins",
    "prefetch_all_parquet_files",
    "preserve_insertion_order",
    "profile_output",
    "profiling_coverage",
    "profiling_mode",
    "profiling_renderer_settings",
    "progress_bar_time",
    "read_ahead_depth",
    "scheduler_process_partial",
    "secret_directory",
    "storage_block_prefetch",
    "storage_compatibility_version",
    "streaming_buffer_size",
    "temp_directory",
    "temp_file_encryption",
    "threads",
    "tracked_metrics",
    "username",
    "vacuum_rebuild_indexes",
    "validate_external_file_cache",
    "variant_minimum_shredding_size",
    "wal_autocheckpoint_entries",
    "write_buffer_row_group_count",
    "write_buffer_row_group_memory_limit",
    "zstd_min_string_length",
];

/// The settings that `SET SESSION` is allowed on.
///
/// Every other one answers `option "x" cannot be set locally`, which is a real error somebody hits
/// rather than a corner: it is what `SET SESSION threads=4` does. DuckDB calls this scope local
/// internally and spells it `SESSION` in SQL, and `SET LOCAL` is a third thing that parses and then
/// says it is not implemented.
const SESSION_SCOPE: &[&str] = &[
    "custom_profiling_settings",
    "debug_force_external",
    "enable_caching_operators",
    "enable_http_logging",
    "enable_profiling",
    "enable_progress_bar",
    "enable_progress_bar_print",
    "http_logging_output",
    "profile_output",
    "profiling_coverage",
    "profiling_mode",
    "progress_bar_time",
    "schema",
    "search_path",
    "streaming_buffer_size",
];

/// The `PRAGMA` names that are a switch rather than a setting, and produce no rows.
///
/// `PRAGMA disable_profiling` is one of these and `SET disable_profiling=1` is an error, which
/// makes this a second namespace rather than a second spelling of the first one.
const PRAGMA_TOGGLES: &[&str] = &[
    "disable_checkpoint_on_shutdown",
    "disable_object_cache",
    "disable_optimizer",
    "disable_print_progress_bar",
    "disable_profile",
    "disable_profiling",
    "disable_progress_bar",
    "disable_verification",
    "disable_verify_external",
    "disable_verify_fetch_row",
    "disable_verify_parallelism",
    "disable_verify_serializer",
    "enable_checkpoint_on_shutdown",
    "enable_object_cache",
    "enable_optimizer",
    "enable_print_progress_bar",
    "enable_profile",
    "enable_profiling",
    "enable_progress_bar",
    "enable_verification",
    "force_checkpoint",
    "verify_external",
    "verify_fetch_row",
    "verify_parallelism",
    "verify_serializer",
];

/// The `PRAGMA` names that produce rows, which are table functions wearing a `PRAGMA` and are not
/// implemented here.
///
/// They are listed so that `PRAGMA table_info('t')` says it is not implemented rather than that it
/// does not exist, which are different things to whoever reads the report.
const PRAGMA_TABLES: &[&str] = &[
    "all_profiling_output",
    "collations",
    "database_list",
    "database_size",
    "extension_versions",
    "functions",
    "metadata_info",
    "platform",
    "show",
    "show_databases",
    "show_tables",
    "show_tables_expanded",
    "storage_info",
    "table_info",
    "user_agent",
    "version",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_is_not_a_setting_is_the_error_duckdb_gives() {
        let error = support_of("nonexistent_thing").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unrecognized configuration parameter \"nonexistent_thing\""),
            "{error}"
        );
    }

    #[test]
    fn a_setting_that_cannot_change_an_answer_is_remembered_and_read_back() {
        let mut settings = Settings::new();
        settings.set("threads", Scope::Unstated, &Value::BigInt(4)).unwrap();
        assert_eq!(settings.get("threads").unwrap(), Value::BigInt(4));
        assert_eq!(settings.get("worker_threads").unwrap(), Value::BigInt(4));
    }

    #[test]
    fn each_pair_of_names_duckdb_calls_an_alias_is_one_setting_and_not_two() {
        for (written, alias) in [
            ("memory_limit", "max_memory"),
            ("wal_autocheckpoint", "checkpoint_threshold"),
            ("profiling_output", "profile_output"),
            ("user", "username"),
            ("null_order", "default_null_order"),
        ] {
            let mut settings = Settings::new();
            // null_order is honoured and takes a fixed few values, so it gets one of them.
            let value = if written == "null_order" {
                Value::Varchar("nulls_first".to_owned())
            } else {
                Value::Varchar("something".to_owned())
            };
            settings.set(written, Scope::Unstated, &value).unwrap();
            assert_eq!(
                settings.get(written).unwrap(),
                settings.get(alias).unwrap(),
                "{written} and {alias}"
            );
        }
    }

    #[test]
    fn a_setting_nobody_has_touched_reads_back_as_null() {
        assert_eq!(Settings::new().get("temp_directory").unwrap(), Value::Null);
    }

    #[test]
    fn a_setting_that_would_change_an_answer_is_refused_rather_than_ignored() {
        let mut settings = Settings::new();
        let value = Value::Varchar("UTC".to_owned());
        let error = settings.set("timezone", Scope::Unstated, &value).unwrap_err();
        assert!(error.to_string().contains("refused rather than ignored"), "{error}");
    }

    #[test]
    fn the_default_null_order_is_last_in_both_directions_which_is_duckdbs_rule() {
        let settings = Settings::new();
        assert_eq!(settings.null_order(), NullOrder::Last);
        assert!(!settings.null_order().first_when(false));
        assert!(!settings.null_order().first_when(true));
        assert!(!settings.order().descending());
    }

    #[test]
    fn every_spelling_duckdb_takes_for_the_null_order_reads_the_same_way_it_does() {
        let mut settings = Settings::new();
        for (written, expected) in [
            ("nulls_first", NullOrder::First),
            ("NULLS FIRST", NullOrder::First),
            ("nulls_last", NullOrder::Last),
            ("sqlite", NullOrder::FirstOnAscending),
            ("mysql", NullOrder::FirstOnAscending),
            ("postgres", NullOrder::LastOnAscending),
            ("nulls_first_on_asc_last_on_desc", NullOrder::FirstOnAscending),
        ] {
            let value = Value::Varchar(written.to_owned());
            settings.set("default_null_order", Scope::Unstated, &value).unwrap();
            assert_eq!(settings.null_order(), expected, "{written}");
            assert_eq!(
                settings.get("null_order").unwrap(),
                Value::Varchar(expected.name().to_owned())
            );
        }
    }

    #[test]
    fn the_two_directional_null_orders_are_the_two_that_depend_on_the_direction() {
        assert!(NullOrder::FirstOnAscending.first_when(false));
        assert!(!NullOrder::FirstOnAscending.first_when(true));
        assert!(!NullOrder::LastOnAscending.first_when(false));
        assert!(NullOrder::LastOnAscending.first_when(true));
    }

    #[test]
    fn a_value_the_setting_does_not_take_is_the_error_duckdb_gives() {
        let mut settings = Settings::new();
        let bogus = Value::Varchar("bogus".to_owned());
        let error = settings.set("default_null_order", Scope::Unstated, &bogus).unwrap_err();
        assert!(error.to_string().contains("expected either NULLS FIRST"), "{error}");
        let error = settings.set("default_order", Scope::Unstated, &bogus).unwrap_err();
        assert!(error.to_string().contains("Expected ASC or DESC"), "{error}");
    }

    #[test]
    fn reset_puts_an_honoured_setting_back_and_is_allowed_on_a_refused_one() {
        let mut settings = Settings::new();
        let desc = Value::Varchar("desc".to_owned());
        settings.set("default_order", Scope::Unstated, &desc).unwrap();
        assert_eq!(settings.order(), DefaultOrder::Descending);
        settings.reset("default_order").unwrap();
        assert_eq!(settings.order(), DefaultOrder::Ascending);
        // Resetting a refused setting is going back to what rudb already does, so it is fine.
        settings.reset("timezone").unwrap();
        assert!(settings.reset("nonexistent_thing").is_err());
    }

    #[test]
    fn a_session_scope_is_refused_on_every_setting_that_does_not_live_in_one() {
        let mut settings = Settings::new();
        let error = settings.set("threads", Scope::Session, &Value::BigInt(4)).unwrap_err();
        assert!(error.to_string().contains("cannot be set locally"), "{error}");
        let mode = Value::Varchar("standard".to_owned());
        settings.set("profiling_mode", Scope::Session, &mode).unwrap();
        settings.set("threads", Scope::Global, &Value::BigInt(4)).unwrap();
    }

    #[test]
    fn set_local_says_what_duckdb_says() {
        let mut settings = Settings::new();
        let error = settings.set("threads", Scope::Local, &Value::BigInt(4)).unwrap_err();
        assert_eq!(error.to_string(), "Not implemented Error: SET LOCAL is not implemented.");
    }

    #[test]
    fn the_pragma_switches_are_their_own_namespace_and_not_the_settings() {
        let mut settings = Settings::new();
        settings.toggle("disable_profiling").unwrap();
        assert!(settings.toggled("DISABLE_PROFILING"));
        assert!(!settings.toggled("enable_profiling"));
        // The same name through SET is an error, which is DuckDB's behaviour and not an oversight.
        let error =
            settings.set("disable_profiling", Scope::Unstated, &Value::BigInt(1)).unwrap_err();
        assert!(error.to_string().contains("unrecognized configuration parameter"), "{error}");
        // And a setting name through PRAGMA on its own is the other error.
        let error = settings.toggle("threads").unwrap_err();
        assert!(error.to_string().contains("Pragma Function with name threads"), "{error}");
    }

    #[test]
    fn a_pragma_that_produces_rows_says_it_is_not_implemented_rather_than_missing() {
        let mut settings = Settings::new();
        let error = settings.toggle("table_info").unwrap_err();
        assert!(error.to_string().contains("produces rows"), "{error}");
    }

    #[test]
    fn no_name_is_in_two_of_the_three_tables() {
        for name in HONOURED {
            assert!(!REFUSED.contains(name), "{name} is both honoured and refused");
            assert!(!KNOWN.contains(name), "{name} is both honoured and remembered");
        }
        for name in REFUSED {
            assert!(!KNOWN.contains(name), "{name} is both refused and remembered");
        }
        for name in SESSION_SCOPE {
            assert!(support_of(name).is_ok(), "{name} takes a session scope and is not a setting");
        }
    }

    #[test]
    fn every_table_is_sorted_and_lowercase_and_holds_no_alias() {
        for table in [HONOURED, REFUSED, KNOWN, SESSION_SCOPE, PRAGMA_TOGGLES, PRAGMA_TABLES] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            assert_eq!(table, sorted.as_slice(), "a table is out of order");
            for name in table {
                assert_eq!(**name, name.to_ascii_lowercase(), "{name} is not lowercase");
            }
        }
        for table in [HONOURED, REFUSED, KNOWN] {
            for name in table {
                assert_eq!(canonical(name), **name, "{name} is an alias and is in a table");
            }
        }
    }

    #[test]
    fn the_settings_the_corpus_uses_most_are_all_classified() {
        // Straight off a count of the upstream corpus. Every one of these has to land somewhere
        // other than the unrecognized parameter error, or the file it is at the top of is lost.
        for name in [
            "default_null_order",
            "force_compression",
            "threads",
            "wal_autocheckpoint",
            "enable_profiling",
            "profiling_output",
            "explain_output",
            "disabled_optimizers",
            "debug_force_external",
            "memory_limit",
            "checkpoint_threshold",
            "immediate_transaction_mode",
            "storage_compatibility_version",
            "max_execution_time",
            "temp_directory",
            "profiling_renderer_settings",
            "operator_memory_limit",
            "tracked_metrics",
            "read_ahead_depth",
        ] {
            assert!(support_of(name).is_ok(), "{name} is not classified");
        }
        // And the same for the ones that only exist as a PRAGMA switch.
        let mut settings = Settings::new();
        for name in [
            "disable_checkpoint_on_shutdown",
            "disable_profiling",
            "verify_parallelism",
            "disable_verification",
            "disable_optimizer",
            "enable_verification",
            "enable_optimizer",
            "force_checkpoint",
        ] {
            settings.toggle(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }
}
