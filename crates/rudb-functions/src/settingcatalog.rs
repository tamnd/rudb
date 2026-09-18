//! What `duckdb_settings()` says about each setting this engine has.
//!
//! A hundred and ninety two rows for a hundred and eighty five settings, because seven of them have a second spelling and each spelling has a row.
//! The names, descriptions, input types, scopes, alias lists and defaults were read from the pinned binary because clients may compare them with the values they already know.
//!
//! # A setting rudb does not read still has to answer
//!
//! rudb acts on twenty three of these names and the other hundred and sixty nine name parts of DuckDB it has no counterpart for.
//! The obvious thing to do with a name the engine does not read is to refuse it, and that was what this table did until the corpus was measured.
//! Refusing them costs 7079 records over 584 files, more files than any other single cause in the run, because a test file sets a knob in its preamble and everything after the refusal goes down with it.
//! So the answer is not one rule but three, and [`Behaviour`] is which of the three a setting gets.
//! A knob cannot change what a query returns, so rudb takes it, keeps it and hands it back, and the query underneath it runs the same either way.
//! Everything else can change an answer, so rudb takes it only at the value it already behaves as and refuses the rest, which keeps `SET preserve_insertion_order = false` an error rather than a promise rudb does not keep.
//!
//! # The alias direction is the opposite way round from the obvious one
//!
//! `max_memory` carries `[memory_limit]` in its alias list and `memory_limit` carries an empty one,
//! and the same for `threads` and `worker_threads`. So the name the documentation uses is the alias
//! and the name nobody types is the entry that points at it. That reads backwards and it is what the
//! binary returns, so it is what is here. Both spellings set the same thing either way, which is the
//! part that matters to a client, and which of the two rows is the one with the list in it only
//! matters to a test.
//!
//! # `typed_value` is a `VARCHAR` here and a `VARIANT` there
//!
//! The pin's last column is a `VARIANT`, which is a type rudb has no [`LogicalType`] for at all, and
//! it holds the same text as `value` on 191 of the pin's 192 rows. So this reports it as a `VARCHAR`
//! with the value in it. Adding a `VARIANT` to the type system for one column of one catalog table
//! would be adding a type no expression can produce, no cast can reach and no file format can store,
//! and the day rudb has a real one this column changes with the rest of them.
//!
//! # What is not here
//!
//! The seam settings, and that is decided in `rudb`'s own settings module rather than in this one.
//! There are twenty seven of them, none is a DuckDB setting, and this table is the answer to "what
//! can I turn that DuckDB also has". `rudb_strategies()` is the table that answers the other
//! question.

use rudb_common::{Field, LogicalType};

/// One setting, and everything `duckdb_settings()` says about it that is not its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingEntry {
    /// The name, as `SET` spells it.
    pub name: &'static str,
    /// The sentence the pin prints, word for word.
    pub description: &'static str,
    /// The type a value for it is read as, which is the pin's spelling and not a [`LogicalType`].
    pub input_type: &'static str,
    /// `GLOBAL` or `LOCAL`, as the pin reports it.
    pub scope: &'static str,
    /// The other spellings of this setting, which the pin fills in on one of the pair and not both.
    pub aliases: &'static [&'static str],
    /// What the engine does with a value handed to this setting.
    pub behaviour: Behaviour,
}

/// What the engine does with a value handed to a setting.
///
/// The three cases are not three degrees of the same thing. The first is a setting rudb reads, the
/// second is one nothing could read because there is nothing for it to change, and the third is one
/// that would need reading and has nowhere yet to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// rudb acts on it. The value lives in the engine rather than in this table, which is why this
    /// case carries none.
    Honoured,
    /// A knob, with the value a database that has set nothing reports for it.
    ///
    /// Nothing it can be set to changes what a query returns, so rudb takes it, keeps it and hands
    /// it back, and the statement after it runs the way it would have anyway. `SET
    /// enable_http_metadata_cache = true` is this: there is no HTTP metadata cache here, there is
    /// nothing a query could notice about whether one is on, and a script that sets it wanted to
    /// keep going rather than to be told rudb has never heard the name.
    Knob(&'static str),
    /// Something rudb does not do, with the one value it already behaves as.
    ///
    /// Setting it to anything else is refused. `SET preserve_insertion_order = false` is this: it is
    /// a real change to what a query returns, rudb cannot make it, and taking the value and not
    /// acting on it would turn one clear error into a wrong answer a statement later.
    DefaultOnly(&'static str),
}

/// The default of a setting the pin answers `NULL` for rather than a value.
///
/// Three of the hundred and ninety two are unset on a fresh connection rather than empty, and the
/// difference is visible twice: `duckdb_settings()` prints null for them where it prints the empty
/// string for the twenty two that really are empty, and `current_setting('parquet_prefetch_column_gap')`
/// is null rather than a number. The three are `enable_profiling`, `operator_memory_limit` and
/// `parquet_prefetch_column_gap`. A NUL byte is what stands for it because it is the one string no
/// `SET` statement can write, so a setting can only be unset by being at its default or by
/// `PRAGMA disable_profiling`, which is the one statement that puts a setting back to nothing.
pub const UNSET: &str = "\0";

/// The scope of a setting that is one per database.
pub const GLOBAL: &str = "GLOBAL";

/// The scope of a setting the pin keeps per connection.
///
/// Fifteen of these rows say `LOCAL` and rudb honours none of the fifteen, so the scope is reported
/// because the pin reports it and not because two connections here can differ.
pub const LOCAL: &str = "LOCAL";

/// Every setting, in the order the pin lists them, which is by name.
pub static SETTINGS: &[SettingEntry] = &[
    SettingEntry {
        name: "Calendar",
        description: "The current calendar",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("gregorian"),
    },
    SettingEntry {
        name: "TimeZone",
        description: "The current time zone",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "__delta_only_variant_encoding_enabled",
        description: "Enables the Parquet reader to identify a Variant structurally.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "access_mode",
        description: "Access mode of the database (AUTOMATIC, READ_ONLY or READ_WRITE)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("automatic"),
    },
    SettingEntry {
        name: "active_grammar_extensions",
        description: "The grammar extensions used by the parser",
        input_type: "VARCHAR[]",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("[]"),
    },
    SettingEntry {
        name: "allocator_background_threads",
        description: "Whether to enable the allocator background thread.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "allocator_bulk_deallocation_flush_threshold",
        description: "If a bulk deallocation larger than this occurs, flush outstanding allocations.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("512.0 MiB"),
    },
    SettingEntry {
        name: "allocator_flush_threshold",
        description: "Peak allocation threshold at which to flush the allocator after completing a task.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("134217728B"),
    },
    SettingEntry {
        name: "allow_community_extensions",
        description: "Allow to load community built extensions",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "allow_extension_repositories",
        description: "Whether custom trusted extension repositories are 'allowed', 'forbidden' (which also distrusts existing repositories) or 'undecided' (the default: blocks adding new repositories, but keeps trusting existing ones). While the database is running the setting can only move from 'undecided' to 'allowed' or 'forbidden', or from 'allowed' to 'forbidden'",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("undecided"),
    },
    SettingEntry {
        name: "allow_extensions_metadata_mismatch",
        description: "Allow to load extensions with not compatible metadata",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "allow_parser_override_extension",
        description: "Allow extensions to override the current parser",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "allow_persistent_secrets",
        description: "Allow the creation of persistent secrets, that are stored and loaded on restarts",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "allow_unredacted_secrets",
        description: "Allow printing unredacted secrets",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "allow_unsigned_extensions",
        description: "Allow to load extensions with invalid or missing signatures",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "allowed_configs",
        description: "List of configuration options that are ALWAYS allowed to be changed - even when lock_configuration is true",
        input_type: "VARCHAR[]",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("[]"),
    },
    SettingEntry {
        name: "allowed_directories",
        description: "List of directories/prefixes that are ALWAYS allowed to be queried - even when enable_external_access is false",
        input_type: "VARCHAR[]",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("[]"),
    },
    SettingEntry {
        name: "allowed_paths",
        description: "List of files that are ALWAYS allowed to be queried - even when enable_external_access is false",
        input_type: "VARCHAR[]",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("[]"),
    },
    SettingEntry {
        name: "approximate_join_order_threshold",
        description: "The minimum number of tables in a join to determine the optimal join order approximately instead of exactly.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("12"),
    },
    SettingEntry {
        name: "arrow_large_buffer_size",
        description: "Whether Arrow buffers for strings, blobs, uuids and bits should be exported using large buffers",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "arrow_lossless_conversion",
        description: "Whenever a DuckDB type does not have a clear native or canonical extension match in Arrow, export the types with a duckdb.type_name extension name.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "arrow_output_list_view",
        description: "Whether export to Arrow format should use ListView as the physical layout for LIST columns",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "arrow_output_version",
        description: "Whether strings should be produced by DuckDB in Utf8View format instead of Utf8",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("1.0"),
    },
    SettingEntry {
        name: "asof_loop_join_threshold",
        description: "The maximum number of rows we need on the left side of an ASOF join to use a nested loop join",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("64"),
    },
    SettingEntry {
        name: "async_threads",
        description: "The number of total async threads used by the system for tasks like I/O.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("128"),
    },
    SettingEntry {
        name: "auto_checkpoint_skip_wal_threshold",
        description: "The estimated WAL write size at which point we will skip writing to the WAL and only checkpoint. Skipping writing to the WAL means concurrent commits are blocked while the checkpoint is happening.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("100000"),
    },
    SettingEntry {
        name: "autoinstall_extension_repository",
        description: "Overrides the custom endpoint for extension installation on autoloading",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "autoinstall_known_extensions",
        description: "Whether known extensions are allowed to be automatically installed when a query depends on them",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "autoload_known_extensions",
        description: "Whether known extensions are allowed to be automatically loaded when a query depends on them",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "binary_as_string",
        description: "In Parquet files, interpret binary data as a string.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "block_allocator_memory",
        description: "Physical memory that the block allocator is allowed to use (this memory is never freed and cannot be reduced).",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0 bytes"),
    },
    SettingEntry {
        name: "cache_local_files",
        description: "Whether the external file cache also caches local files (remote files are always cached)",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "catalog_error_max_schemas",
        description: "The maximum number of schemas the system will scan for \"did you mean...\" style errors in the catalog",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("100"),
    },
    SettingEntry {
        name: "checkpoint_on_detach",
        description: "Override checkpoint behavior when detaching a database. ENABLED requests a checkpoint, but the checkpoint does not occur if another connection still references the database. DISABLED never checkpoints, DEFAULT defers to the global checkpoint_on_shutdown setting.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("DEFAULT"),
    },
    SettingEntry {
        name: "checkpoint_threshold",
        description: "The WAL size threshold at which to automatically trigger a checkpoint (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &["wal_autocheckpoint"],
        behaviour: Behaviour::Knob("16.0 MiB"),
    },
    SettingEntry {
        name: "current_dialect",
        description: "The SQL dialect used by the parser",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "current_transaction_invalidation_policy",
        description: "Which types of exceptions invalidate the database for the current transaction",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("STANDARD_POLICY"),
    },
    SettingEntry {
        name: "custom_extension_repository",
        description: "Overrides the custom endpoint for remote extension installation",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "custom_user_agent",
        description: "Metadata from DuckDB callers",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "debug_asof_iejoin",
        description: "DEBUG SETTING: force use of IEJoin to implement AsOf joins",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_checkpoint_abort",
        description: "DEBUG SETTING: trigger an abort while checkpointing for testing purposes",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("NONE"),
    },
    SettingEntry {
        name: "debug_checkpoint_sleep_ms",
        description: "DEBUG SETTING: time to sleep before a checkpoint",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0"),
    },
    SettingEntry {
        name: "debug_disable_optimizer",
        description: "DEBUG SETTING: disable optimizer for most queries",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "debug_eviction_queue_sleep_micro_seconds",
        description: "DEBUG SETTING: time for the eviction queue to sleep before acquiring shared ownership of block memory",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0"),
    },
    SettingEntry {
        name: "debug_force_commit_failure",
        description: "DEBUG SETTING: force transaction commit to fail after the undo buffer has been committed, used for testing commit error recovery",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "debug_force_commit_revert_failure",
        description: "DEBUG SETTING: force RevertCommit to fail while recovering from a commit failure, used for testing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "debug_force_external",
        description: "DEBUG SETTING: force out-of-core computation for operators that support it, used for testing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_force_fetch_row",
        description: "DEBUG SETTING: force per-row fetching during scans, used for testing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_force_no_cross_product",
        description: "DEBUG SETTING: Force disable cross product generation when hyper graph isn't connected, used for testing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_local_file_system_delay_ms",
        description: "DEBUG SETTING: time to sleep before local file system open/read/write operations",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0"),
    },
    SettingEntry {
        name: "debug_order_verification",
        description: "DEBUG SETTING: verify ORDER BY results by rewriting the ordering (NONE, CREATE_SORT_KEY or VARIANT)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("none"),
    },
    SettingEntry {
        name: "debug_physical_table_scan_execution_strategy",
        description: "DEBUG SETTING: force use of given strategy for executing physical table scans",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("DEFAULT"),
    },
    SettingEntry {
        name: "debug_skip_checkpoint_on_commit",
        description: "DEBUG SETTING: skip checkpointing on commit",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "debug_transformer_trampoline_style",
        description: "Use the experimental trampoline-style parser transformer",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verification_mode",
        description: "DEBUG SETTING: toggle the verification mode.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("NONE"),
    },
    SettingEntry {
        name: "debug_verification_projection",
        description: "DEBUG SETTING: add internal verification projections to stress optimizers",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_aggregate_state_export",
        description: "DEBUG SETTING: enable verification of aggregate state export",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_blocks",
        description: "DEBUG SETTING: verify block metadata during checkpointing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_column_bindings",
        description: "DEBUG SETTING: run extra internal verification of column bindings",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_serializer",
        description: "DEBUG SETTING: verify logical plan serializer",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_statement",
        description: "DEBUG SETTING: the type of statement verification to perform",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("NONE"),
    },
    SettingEntry {
        name: "debug_verify_stats",
        description: "DEBUG SETTING: verify statistics are correct during execution, instead of assuming",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "debug_verify_vector",
        description: "DEBUG SETTING: enable vector verification",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("NONE"),
    },
    SettingEntry {
        name: "debug_window_mode",
        description: "DEBUG SETTING: switch window mode to use",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("WINDOW"),
    },
    SettingEntry {
        name: "default_block_size",
        description: "The default block size for new duckdb database files (new as-in, they do not yet exist).",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("262144"),
    },
    SettingEntry {
        name: "default_collation",
        description: "The collation setting used when none is specified",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly(""),
    },
    SettingEntry {
        name: "default_null_order",
        description: "NULL ordering used when none is specified (NULLS_FIRST or NULLS_LAST)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &["null_order"],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "default_order",
        description: "The order type used when none is specified (ASC or DESC)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "default_secret_storage",
        description: "Allows switching the default storage for secrets",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "default_transaction_invalidation_policy",
        description: "When to invalidate transactions when errors occur (SYNTACTIC_ERRORS_DO_NOT_INVALIDATE, i.e. parser and binder exceptions do not invalidate, or ALL_ERRORS_INVALIDATE_TRANSACTION)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("ALL_ERRORS_INVALIDATE_TRANSACTION"),
    },
    SettingEntry {
        name: "delim_join_as_cte",
        description: "Rewrite delim joins to materialized CTEs during dependent join flattening",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "dialect_compatibility_mode",
        description: "Enable SQL dialect compatibility for a certain engine (e.g. `SET dialect_compatibility_mode='spark'`)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "disable_database_invalidation",
        description: "Disables invalidating the database instance when encountering a fatal error. Should be used with great care, as DuckDB cannot guarantee correct behavior after a fatal error.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "disable_parquet_prefetching",
        description: "Disable the prefetching mechanism in Parquet",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "disable_timestamptz_casts",
        description: "Disable casting from timestamp to timestamptz ",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "disabled_compression_methods",
        description: "Disable a specific set of compression methods (comma separated)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "disabled_filesystems",
        description: "Disable specific file systems preventing access (e.g. LocalFileSystem)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly(""),
    },
    SettingEntry {
        name: "disabled_log_types",
        description: "Sets the list of disabled loggers",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "disabled_optimizers",
        description: "DEBUG SETTING: disable a specific set of optimizers (comma separated)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "duckdb_api",
        description: "DuckDB API surface",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("rudb"),
    },
    SettingEntry {
        name: "dynamic_or_filter_threshold",
        description: "The maximum amount of OR filters we generate dynamically from a hash join",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("50"),
    },
    SettingEntry {
        name: "enable_external_access",
        description: "Allow the database to access external state (through e.g. loading/installing modules, COPY TO/FROM, CSV readers, pandas replacement scans, etc)",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("true"),
    },
    SettingEntry {
        name: "enable_external_file_cache",
        description: "Allow the database to cache external files (e.g., Parquet) in memory.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "enable_fsst_vectors",
        description: "Allow scans on FSST compressed segments to emit compressed vectors to utilize late decompression",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "enable_geoparquet_conversion",
        description: "Attempt to decode/encode geometry data in/as GeoParquet files if the spatial extension is present.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "enable_http_metadata_cache",
        description: "Whether or not the global http metadata is used to cache HTTP metadata",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "enable_logging",
        description: "Enables the logger",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("1"),
    },
    SettingEntry {
        name: "enable_macro_dependencies",
        description: "Enable created MACROs to create dependencies on the referenced objects (such as tables)",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "enable_object_cache",
        description: "[PLACEHOLDER] Legacy setting - does nothing",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "enable_optimistic_write",
        description: "Whether or not to optimistically write large appends to disk before committing. Disable this to keep bulk appends in memory (e.g. for in-memory benchmarks).",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "enable_optimizer",
        description: "Whether or not query optimization is enabled",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "enable_profiling",
        description: "Enables profiling, and sets the output format (JSON, QUERY_TREE, QUERY_TREE_OPTIMIZER)",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob(UNSET),
    },
    SettingEntry {
        name: "enable_progress_bar",
        description: "Enables the progress bar, printing progress to the terminal for long queries",
        input_type: "BOOLEAN",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "enable_progress_bar_print",
        description: "Controls the printing of the progress bar, when 'enable_progress_bar' is true",
        input_type: "BOOLEAN",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "enable_view_dependencies",
        description: "Enable created VIEWs to create dependencies on the referenced objects (such as tables)",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "enabled_log_types",
        description: "Sets the list of enabled loggers",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "errors_as_json",
        description: "Output error messages as structured JSON instead of as a raw string",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "experimental_metadata_reuse",
        description: "EXPERIMENTAL: Re-use row group and table metadata when checkpointing.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "explain_output",
        description: "Output of EXPLAIN statements (ALL, OPTIMIZED_ONLY, PHYSICAL_ONLY)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("PHYSICAL_ONLY"),
    },
    SettingEntry {
        name: "extension_directories",
        description: "Set the directories to store extensions in",
        input_type: "VARCHAR[]",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("[]"),
    },
    SettingEntry {
        name: "extension_repository_directory",
        description: "Set the directory in which trusted extension repositories are stored. This is the trust anchor for user-provided repositories, so while signature checking is enabled (allow_unsigned_extensions=false) it can only be set at startup, not while the database is running",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "external_file_cache_local_block_size",
        description: "Block size in bytes for the external file cache when reading local (non-remote) files.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("16384"),
    },
    SettingEntry {
        name: "external_file_cache_remote_block_size",
        description: "Block size in bytes for the external file cache when reading remote files (e.g. HTTP/S3).",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("2097152"),
    },
    SettingEntry {
        name: "external_file_cache_spill",
        description: "Whether evicted external file cache blocks of remote files spill to the temporary directory instead of being dropped, so that they are re-read from there rather than re-fetched from the source",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "external_threads",
        description: "The number of external threads that work on DuckDB tasks.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("1"),
    },
    SettingEntry {
        name: "file_search_path",
        description: "A comma separated list of directories to search for input files",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "force_column_metadata_reuse",
        description: "Force re-use of row group metadata on a column-level when checkpointing on older storage versions 6 and 7. This breaks storage backward-compatibility with older DuckDB versions.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "force_compression",
        description: "DEBUG SETTING: forces a specific compression method to be used",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("auto"),
    },
    SettingEntry {
        name: "geometry_minimum_shredding_size",
        description: "Minimum size of a rowgroup to enable GEOMETRY shredding, or set to -1 to disable entirely. Defaults to 1/4th of a rowgroup",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("30000"),
    },
    SettingEntry {
        name: "heap_based_parser",
        description: "Use the heap-based PEG parser",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("true"),
    },
    SettingEntry {
        name: "home_directory",
        description: "Sets the home directory used by the system",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "http_proxy",
        description: "HTTP proxy host (defaults to the HTTP_PROXY environment variable when unset)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "http_proxy_password",
        description: "Password for HTTP proxy",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "http_proxy_username",
        description: "Username for HTTP proxy",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "ieee_floating_point_ops",
        description: "Use IEEE 754 behavior for supported floating point operations, returning NAN/INF instead of errors/NULL.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "ignore_unknown_crs",
        description: "Ignore unknown Coordinate Reference Systems (CRS) when creating geometry types or importing geospatial data.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "immediate_transaction_mode",
        description: "Whether transactions should be started lazily when needed, or immediately when BEGIN TRANSACTION is called",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "index_scan_max_count",
        description: "The maximum index scan count sets a threshold for index scans. If fewer than MAX(index_scan_max_count, index_scan_percentage * total_row_count) rows match, we perform an index scan instead of a table scan.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("2048"),
    },
    SettingEntry {
        name: "index_scan_percentage",
        description: "The index scan percentage sets a threshold for index scans. If fewer than MAX(index_scan_max_count, index_scan_percentage * total_row_count) rows match, we perform an index scan instead of a table scan.",
        input_type: "DOUBLE",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0.001"),
    },
    SettingEntry {
        name: "initial_column_segment_size",
        description: "The initial memory (in bytes) reserved for the first transient column segment. Must be a power of two. Internally, we subtract the block header size (typically 8 bytes) for segments with or exceeding 1024 bytes. Subsequent segments double in size until reaching the block size.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("2048"),
    },
    SettingEntry {
        name: "integer_division",
        description: "Whether or not the / operator defaults to integer division, or to floating point division",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "json_geometry_format",
        description: "How GEOMETRY values are written to JSON: 'wkt' for Well-Known Text, or 'geojson' for GeoJSON geometry objects. COPY ... TO ... (FORMAT GEOJSON) always writes GeoJSON regardless of this setting.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("wkt"),
    },
    SettingEntry {
        name: "lambda_syntax",
        description: "Configures the use of the deprecated single arrow operator (->) for lambda functions.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("DEFAULT"),
    },
    SettingEntry {
        name: "late_materialization_max_rows",
        description: "The maximum amount of rows in the LIMIT/SAMPLE for which we trigger late materialization",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("50"),
    },
    SettingEntry {
        name: "legacy_disable_null_type",
        description: "When enabled, prevent the NULL type from leaving the binder (< v2.0 default behavior)",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "legacy_metrics_format",
        description: "When enabled, profiling output uses the legacy flat format instead of the current grouped format",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "lock_configuration",
        description: "Whether or not configurations can be altered",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("false"),
    },
    SettingEntry {
        name: "log_query_path",
        description: "Specifies the path to which queries should be logged (default: NULL, queries are not logged)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "logging_level",
        description: "The log level which will be recorded in the log",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("WARNING"),
    },
    SettingEntry {
        name: "logging_mode",
        description: "Determines which types of log messages are logged",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("LEVEL_ONLY"),
    },
    SettingEntry {
        name: "logging_storage",
        description: "Set the logging storage (memory/stdout/file/<custom>)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("shell_log_storage"),
    },
    SettingEntry {
        name: "max_execution_time",
        description: "The maximum execution time per query in milliseconds (0 = no limit)",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("0"),
    },
    SettingEntry {
        name: "max_expression_depth",
        description: "The maximum expression depth limit in the parser. WARNING: increasing this setting and using very deep expressions might lead to stack overflow errors.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("1000"),
    },
    SettingEntry {
        name: "max_memory",
        description: "The maximum memory of the system (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &["memory_limit"],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "max_streaming_buffer_size",
        description: "The maximum number of bytes a streaming query result buffers (e.g. 1GB). Buffered bytes stay under this cap plus at most one chunk: an oversized chunk is only admitted into an empty queue",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &["streaming_buffer_size"],
        behaviour: Behaviour::Knob("10.0 MiB"),
    },
    SettingEntry {
        name: "max_temp_directory_size",
        description: "The maximum amount of data stored inside the 'temp_directory' (when set) (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("unlimited"),
    },
    SettingEntry {
        name: "max_vacuum_tasks",
        description: "The maximum vacuum tasks to schedule during a checkpoint.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("100"),
    },
    SettingEntry {
        name: "memory_limit",
        description: "The maximum memory of the system (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "merge_join_threshold",
        description: "The maximum number of rows on either table to choose a merge join",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("1000"),
    },
    SettingEntry {
        name: "nested_loop_join_threshold",
        description: "The maximum number of rows on either table to choose a nested loop join",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("5"),
    },
    SettingEntry {
        name: "null_on_division_by_zero",
        description: "Return NULL instead of throwing an error when dividing by zero.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "null_order",
        description: "NULL ordering used when none is specified (NULLS_FIRST or NULLS_LAST)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "operator_memory_limit",
        description: "The maximum memory for query intermediates (sorts, hash tables) per connection (e.g. 256MB)",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob(UNSET),
    },
    SettingEntry {
        name: "order_by_non_integer_literal",
        description: "Allow ordering by non-integer literals - ordering by such literals has no effect.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "ordered_aggregate_threshold",
        description: "The number of rows to accumulate before sorting, used for tuning",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("262144"),
    },
    SettingEntry {
        name: "parquet_metadata_cache",
        description: "Cache Parquet metadata - useful when reading the same files multiple times",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "parquet_prefetch_column_gap",
        description: "Byte gap under which Parquet prefetch I/O ranges are coalesced (NULL lets the cost model adapt it)",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(UNSET),
    },
    SettingEntry {
        name: "partitioned_write_flush_threshold",
        description: "The threshold in number of rows after which we flush a thread state when writing using PARTITION_BY",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("524288"),
    },
    SettingEntry {
        name: "partitioned_write_max_open_files",
        description: "The maximum amount of files the system can keep open before flushing to disk when writing using PARTITION_BY",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("100"),
    },
    SettingEntry {
        name: "password",
        description: "The password to use. Ignored for legacy compatibility.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "perfect_ht_threshold",
        description: "Threshold in bytes for when to use a perfect hash table",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("12"),
    },
    SettingEntry {
        name: "pin_threads",
        description: "Whether to pin threads to cores (Linux only, default AUTO: on when there are more than 64 cores)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("auto"),
    },
    SettingEntry {
        name: "pivot_filter_threshold",
        description: "The threshold to switch from using filtered aggregates to LIST with a dedicated pivot operator",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("20"),
    },
    SettingEntry {
        name: "pivot_limit",
        description: "The maximum number of pivot columns in a pivot statement",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("100000"),
    },
    SettingEntry {
        name: "prefer_range_joins",
        description: "Force use of range joins with mixed predicates",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "prefetch_all_parquet_files",
        description: "(deprecated) Parquet files are now always prefetched, this setting has no effect",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "preserve_identifier_case",
        description: "How to fold non-quoted identifiers: 'preserve_case' keeps the case as written, 'lowercase' lowercases them, 'uppercase' uppercases them",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "preserve_insertion_order",
        description: "Whether or not to preserve insertion order. If set to false the system is allowed to re-order any results that do not contain ORDER BY clauses.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("true"),
    },
    SettingEntry {
        name: "profile_output",
        description: "The file to which profile output should be saved, or empty to print to the terminal",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "profiling_coverage",
        description: "The profiling coverage (SELECT or ALL)",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("SELECT"),
    },
    SettingEntry {
        name: "profiling_output",
        description: "The file to which profile output should be saved, or empty to print to the terminal",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &["profile_output"],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "profiling_renderer_settings",
        description: "A map of settings passed to the renderer of the profiler output (e.g. {'max_extra_lines': 100}) - settings not recognized by the active renderer are ignored",
        input_type: "MAP(VARCHAR, VARCHAR)",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("{}"),
    },
    SettingEntry {
        name: "progress_bar_time",
        description: "Sets the time (in milliseconds) how long a query needs to take before we start printing a progress bar",
        input_type: "BIGINT",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("2000"),
    },
    SettingEntry {
        name: "read_ahead_depth",
        description: "Number of scan jobs prefetched ahead of decoding. -1 = automatic (backlog bounded by a memory budget), 0 = disabled.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("-1"),
    },
    SettingEntry {
        name: "regex_match_operator_semantics",
        description: "Configures whether regex match operators use partial or full string matching",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "scalar_subquery_error_on_multiple_rows",
        description: "Throw an error when a scalar subquery returns more than one row. When disabled, an arbitrary row is returned instead.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "scheduler_process_partial",
        description: "Partially process tasks before rescheduling - allows for more scheduler fairness between separate queries",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "schema",
        description: "Sets the default search schema. Equivalent to setting search_path to a single value.",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("main"),
    },
    SettingEntry {
        name: "search_path",
        description: "Sets the default catalog search path as a comma-separated list of values",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly(""),
    },
    SettingEntry {
        name: "secret_directory",
        description: "Set the directory to which persistent secrets are stored",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "show_behavior",
        description: "How SHOW resolves a bare identifier: 'auto' (describe a table if one exists, else a setting; deprecated), 'table' (always a table), or 'setting' (always a setting)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "standard_vector_size",
        description: "The compiled-in STANDARD_VECTOR_SIZE (read-only)",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("2048"),
    },
    SettingEntry {
        name: "storage_block_prefetch",
        description: "In which scenarios to use storage block prefetching",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("REMOTE_ONLY"),
    },
    SettingEntry {
        name: "storage_compatibility_version",
        description: "Serialize on checkpoint with compatibility for a given duckdb version",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("latest"),
    },
    SettingEntry {
        name: "streaming_buffer_size",
        description: "The maximum number of bytes a streaming query result buffers (e.g. 1GB). Buffered bytes stay under this cap plus at most one chunk: an oversized chunk is only admitted into an empty queue",
        input_type: "VARCHAR",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("10.0 MiB"),
    },
    SettingEntry {
        name: "table_function_identifier_conversion",
        description: "Configures the use of deprecated implicit conversion of unbound identifiers to strings in table function arguments.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::DefaultOnly("DEFAULT"),
    },
    SettingEntry {
        name: "temp_directory",
        description: "Set the directory to which to write temp files",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "temp_file_encryption",
        description: "Encrypt all temporary files if database is encrypted",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("false"),
    },
    SettingEntry {
        name: "threads",
        description: "The number of total threads used by the system.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &["worker_threads"],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "tracked_metrics",
        description: "A list of metric glob patterns to enable for collection (e.g. ['query.*', 'optimizer.*'])",
        input_type: "VARCHAR[]",
        scope: LOCAL,
        aliases: &[],
        behaviour: Behaviour::Knob("[*]"),
    },
    SettingEntry {
        name: "user",
        description: "The username to use. Ignored for legacy compatibility.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "username",
        description: "The username to use. Ignored for legacy compatibility.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &["user"],
        behaviour: Behaviour::Knob(""),
    },
    SettingEntry {
        name: "vacuum_rebuild_indexes",
        description: "(Experimental) Allow vacuum to compact row groups on tables with bound ART indexes, rebuilding the indexes afterward. Tables with a row count exceeding this threshold are skipped. 0 = disabled. Can also be set per-database via the 'vacuum_rebuild_indexes' ATTACH option, which overrides this default.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0"),
    },
    SettingEntry {
        name: "validate_external_file_cache",
        description: "Cache validation mode: VALIDATE_ALL (default, validate all cache entries), VALIDATE_REMOTE (validate only remote cache entries), or NO_VALIDATION (disable cache validation).",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("VALIDATE_ALL"),
    },
    SettingEntry {
        name: "variant_minimum_shredding_size",
        description: "Minimum size of a rowgroup to enable VARIANT shredding, or set to -1 to disable entirely. Defaults to 1/4th of a rowgroup",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("30000"),
    },
    SettingEntry {
        name: "wal_autocheckpoint",
        description: "The WAL size threshold at which to automatically trigger a checkpoint (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("16.0 MiB"),
    },
    SettingEntry {
        name: "wal_autocheckpoint_entries",
        description: "Trigger automatic checkpoint when WAL entry count reaches or exceeds N (0 = disabled)",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("0"),
    },
    SettingEntry {
        name: "warnings_as_errors",
        description: "Escalate all warnings to errors.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "worker_threads",
        description: "The number of total threads used by the system.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Honoured,
    },
    SettingEntry {
        name: "write_buffer_row_group_count",
        description: "The amount of row groups to buffer in bulk ingestion prior to flushing them together. Reducing this setting can reduce memory consumption.",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("5"),
    },
    SettingEntry {
        name: "write_buffer_row_group_memory_limit",
        description: "The maximum data to buffer in row groups (in bytes) to buffer prior to flushing them together. When either this limit is reached, or write_buffer_row_group_count is reached, we flush the data to disk. Defaults to 20% of memory limit divided by thread count.",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("155.5 MiB"),
    },
    SettingEntry {
        name: "zstd_min_string_length",
        description: "The (average) length at which to enable ZSTD compression, defaults to 4096",
        input_type: "UBIGINT",
        scope: GLOBAL,
        aliases: &[],
        behaviour: Behaviour::Knob("4096"),
    },
];

/// The columns `duckdb_settings()` returns, in the pin's order.
#[must_use]
pub fn setting_fields() -> Vec<Field> {
    vec![
        Field::new("name", LogicalType::Varchar),
        Field::new("value", LogicalType::Varchar),
        Field::new("description", LogicalType::Varchar),
        Field::new("input_type", LogicalType::Varchar),
        Field::new("scope", LogicalType::Varchar),
        Field::new("aliases", LogicalType::list(LogicalType::Varchar)),
        Field::new("typed_value", LogicalType::Varchar),
    ]
}

/// The entry for a setting with this name, and `None` for a name that is not a setting.
///
/// The comparison ignores case, which is the pin's rule rather than a convenience here.
/// `SELECT current_setting('THREADS')` answers with the thread count there and `SET THREADS = 4`
/// turns it, so a setting name is matched the way an identifier is and not the way a string is.
#[must_use]
pub fn setting_named(name: &str) -> Option<&'static SettingEntry> {
    SETTINGS.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
}

/// What the engine says when it is handed a name that is not a setting.
///
/// Here rather than where each caller is, because there are three of them and they are in two
/// crates. `SET nope = 1`, `RESET nope` and `current_setting('nope')` all say this, and on the pin
/// they say the same sentence as each other, so one sentence is what they share.
///
/// The list after it is the five nearest names, the way the pin prints at most five, because with a
/// hundred and ninety two of them printing the lot buries the one the writer meant. The pin scores
/// by its own string similarity and this scores by edit distance, so the two lists agree on the
/// obvious misspellings and may differ on the rest.
#[must_use]
pub fn unknown_setting(name: &str) -> String {
    let mut scored: Vec<(usize, &'static str)> =
        SETTINGS.iter().map(|entry| (distance(name, entry.name), entry.name)).collect();
    // Ties go to the name that sorts first, so the list is the same one twice for the same input.
    scored.sort_unstable();
    let near: Vec<String> = scored
        .iter()
        .take(SUGGESTIONS)
        .filter(|(score, candidate)| *score <= cutoff(name, candidate))
        .map(|(_, candidate)| format!("\"{candidate}\""))
        .collect();
    let message = format!("unrecognized configuration parameter \"{name}\"");
    if near.is_empty() {
        return message;
    }
    format!("{message}\n\nDid you mean: {}", near.join(", "))
}

/// How many names the suggestion list holds at most, which is what the pin prints.
const SUGGESTIONS: usize = 5;

/// How far a name may be from what was written and still be worth suggesting.
///
/// A third of the longer of the two, so a short name has to be nearly right and a long one may be
/// off by several letters. Without a cutoff a name like `x` would drag in whichever five settings
/// happen to be shortest, which is a list about the table rather than about the mistake. A third
/// rather than a half because at a half `memory_limitt` suggests `pivot_limit`, which shares a
/// suffix and nothing else.
fn cutoff(written: &str, candidate: &str) -> usize {
    written.chars().count().max(candidate.chars().count()).div_ceil(3).max(1)
}

/// The number of single character edits between two names, ignoring case.
///
/// The ordinary two row Levenshtein. Both strings are names a person typed or a table holds, so
/// this walks characters rather than bytes and a multi byte letter counts once.
fn distance(written: &str, candidate: &str) -> usize {
    let left: Vec<char> = written.chars().flat_map(char::to_lowercase).collect();
    let right: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, from) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, to) in right.iter().enumerate() {
            let substitute = previous[column] + usize::from(from != to);
            current[column + 1] = substitute.min(previous[column + 1] + 1).min(current[column] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::{
        Behaviour, GLOBAL, LOCAL, SETTINGS, setting_fields, setting_named, unknown_setting,
    };

    #[test]
    fn the_table_is_the_shape_the_pin_returns() {
        assert_eq!(SETTINGS.len(), 192, "a hundred and eighty five settings, seven of them twice");
        assert_eq!(setting_fields().len(), 7);
    }

    #[test]
    fn the_names_are_sorted_because_the_pin_returns_them_that_way() {
        let names: Vec<&str> = SETTINGS.iter().map(|entry| entry.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    /// The alias reads backwards, so it gets a test rather than a comment nobody checks against the
    /// binary again.
    #[test]
    fn an_alias_is_a_row_of_its_own_and_the_list_sits_on_the_other_one() {
        let memory = setting_named("max_memory").expect("a setting");
        assert_eq!(memory.aliases, ["memory_limit"]);
        assert_eq!(setting_named("memory_limit").expect("a setting").aliases, [] as [&str; 0]);
        let threads = setting_named("threads").expect("a setting");
        assert_eq!(threads.aliases, ["worker_threads"]);
        assert_eq!(setting_named("worker_threads").expect("a setting").aliases, [] as [&str; 0]);
        // Both halves of a pair say the same thing, since they are one setting with two names.
        assert_eq!(
            memory.description,
            setting_named("memory_limit").expect("a setting").description
        );
        assert_eq!(
            threads.input_type,
            setting_named("worker_threads").expect("a setting").input_type
        );
        // Seven pairs, and every name in a list is a row of its own.
        let pairs: Vec<&str> =
            SETTINGS.iter().filter(|entry| !entry.aliases.is_empty()).map(|e| e.name).collect();
        assert_eq!(pairs.len(), 7, "{pairs:?}");
        for entry in SETTINGS {
            for alias in entry.aliases {
                let other = setting_named(alias).expect("an alias has a row");
                assert_eq!(other.behaviour, entry.behaviour, "{}", entry.name);
                assert_eq!(other.scope, entry.scope, "{}", entry.name);
            }
        }
    }

    #[test]
    fn every_scope_is_one_of_the_two_the_pin_prints() {
        let local = SETTINGS.iter().filter(|entry| entry.scope == LOCAL).count();
        assert_eq!(local, 15);
        for entry in SETTINGS {
            assert!(entry.scope == GLOBAL || entry.scope == LOCAL, "{}", entry.name);
        }
        assert_eq!(setting_named("nothing_called_this"), None);
    }

    /// Twenty three names are read by the engine and the rest are taken and kept, or taken at one
    /// value and refused at the others. The counts are here so that moving a setting from one case
    /// to another is a line in a diff rather than something nobody notices.
    #[test]
    fn every_setting_is_read_or_carried_or_held_at_its_default() {
        let count = |wanted: fn(&Behaviour) -> bool| {
            SETTINGS.iter().filter(|entry| wanted(&entry.behaviour)).count()
        };
        assert_eq!(count(|b| matches!(b, Behaviour::Honoured)), 23);
        assert_eq!(count(|b| matches!(b, Behaviour::Knob(_))), 133);
        assert_eq!(count(|b| matches!(b, Behaviour::DefaultOnly(_))), 36);
        assert_eq!(
            setting_named("memory_limit").expect("a setting").behaviour,
            Behaviour::Honoured
        );
        assert_eq!(
            setting_named("enable_http_metadata_cache").expect("a setting").behaviour,
            Behaviour::Knob("false")
        );
        assert_eq!(
            setting_named("preserve_insertion_order").expect("a setting").behaviour,
            Behaviour::DefaultOnly("true")
        );
    }

    /// The pin answers `current_setting('THREADS')` and turns `SET THREADS`, so case is ignored.
    #[test]
    fn a_setting_is_found_whichever_way_the_name_is_cased() {
        assert_eq!(setting_named("THREADS").expect("a setting").name, "threads");
        assert_eq!(setting_named("Memory_Limit").expect("a setting").name, "memory_limit");
    }

    /// The sentence three callers in two crates share, with the pin's blank line in the middle.
    #[test]
    fn an_unknown_setting_is_named_and_then_the_nearest_ones_are_listed() {
        // Word for word what the pin says to the same mistake.
        let message = unknown_setting("memory_limitt");
        assert_eq!(
            message,
            "unrecognized configuration parameter \"memory_limitt\"\n\nDid you mean: \"memory_limit\""
        );
        // At most five, the way the pin prints at most five.
        let many = unknown_setting("enable_");
        assert!(many.matches('"').count() <= 2 + 5 * 2, "{many}");
        // A name nothing is near gets the sentence and no list, rather than five names picked for
        // being short.
        assert_eq!(
            unknown_setting("zzzzzzzzzzzzzzzzzzzzzzzz"),
            "unrecognized configuration parameter \"zzzzzzzzzzzzzzzzzzzzzzzz\""
        );
    }
}
