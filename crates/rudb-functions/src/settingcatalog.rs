//! What `duckdb_settings()` says about each setting this engine has.
//!
//! Fourteen rows represent twelve settings because two aliases have rows of their own.
//! The descriptions, input types, and alias lists were read from the pinned binary because clients may compare them with the values they already know.
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
//! The pin returns 192 rows and this returns 14, because rudb has twelve settings.
//! The other settings are for things rudb does not do, and a row saying `SET enable_http_metadata_cache = true` worked when nothing read it would be worse than no row at all.
//! The list grows when the engine does.
//!
//! The seam settings are not here either, and that is decided in `rudb`'s own settings module rather
//! than in this one. There are twenty seven of them, none is a DuckDB setting, and this table is the
//! answer to "what can I turn that DuckDB also has". `rudb_strategies()` is the table that answers
//! the other question.

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
    /// `GLOBAL` or `LOCAL`, and every setting rudb has is global.
    pub scope: &'static str,
    /// The other spellings of this setting, which the pin fills in on one of the pair and not both.
    pub aliases: &'static [&'static str],
}

/// The scope every setting rudb has, since none of them is per connection yet.
pub const GLOBAL: &str = "GLOBAL";

/// Every setting, in the order the pin lists them, which is by name.
pub static SETTINGS: &[SettingEntry] = &[
    SettingEntry {
        name: "TimeZone",
        description: "The current time zone",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "default_null_order",
        description: "NULL ordering used when none is specified (NULLS_FIRST or NULLS_LAST)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "default_order",
        description: "The order type used when none is specified (ASC or DESC)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "disable_timestamptz_casts",
        description: "Disable casting from timestamp to timestamptz ",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "disabled_optimizers",
        description: "DEBUG SETTING: disable a specific set of optimizers (comma separated)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "ieee_floating_point_ops",
        description: "Use IEEE 754 behavior for supported floating point operations, returning NAN/INF instead of errors/NULL.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "integer_division",
        description: "Whether or not the / operator defaults to integer division, or to floating point division",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "max_memory",
        description: "The maximum memory of the system (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &["memory_limit"],
    },
    SettingEntry {
        name: "memory_limit",
        description: "The maximum memory of the system (e.g. 1GB)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "null_on_division_by_zero",
        description: "Return NULL instead of throwing an error when dividing by zero.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "order_by_non_integer_literal",
        description: "Allow ordering by non-integer literals - ordering by such literals has no effect.",
        input_type: "BOOLEAN",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "regex_match_operator_semantics",
        description: "Configures whether regex match operators use partial or full string matching",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "show_behavior",
        description: "How SHOW resolves a bare identifier: 'auto' (describe a table if one exists, else a setting; deprecated), 'table' (always a table), or 'setting' (always a setting)",
        input_type: "VARCHAR",
        scope: GLOBAL,
        aliases: &[],
    },
    SettingEntry {
        name: "threads",
        description: "The number of total threads used by the system.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &["worker_threads"],
    },
    SettingEntry {
        name: "worker_threads",
        description: "The number of total threads used by the system.",
        input_type: "BIGINT",
        scope: GLOBAL,
        aliases: &[],
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
/// The list after it is upstream's suggestion list, which on the pin is the five nearest names by edit distance out of its hundred and ninety two.
/// This prints every setting rudb has, since a complete list is more useful while that set is still small.
#[must_use]
pub fn unknown_setting(name: &str) -> String {
    let known: Vec<String> = SETTINGS.iter().map(|entry| format!("\"{}\"", entry.name)).collect();
    format!("unrecognized configuration parameter \"{name}\"\n\nDid you mean: {}", known.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{GLOBAL, SETTINGS, setting_fields, setting_named, unknown_setting};

    #[test]
    fn the_table_is_the_shape_the_pin_returns() {
        assert_eq!(SETTINGS.len(), 15, "thirteen settings and two of them have a second spelling");
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
    }

    #[test]
    fn nothing_here_is_per_connection_yet_and_the_table_says_so() {
        for entry in SETTINGS {
            assert_eq!(entry.scope, GLOBAL, "{}", entry.name);
        }
        assert_eq!(setting_named("nothing_called_this"), None);
    }

    /// The pin answers `current_setting('THREADS')` and turns `SET THREADS`, so case is ignored.
    #[test]
    fn a_setting_is_found_whichever_way_the_name_is_cased() {
        assert_eq!(setting_named("THREADS").expect("a setting").name, "threads");
        assert_eq!(setting_named("Memory_Limit").expect("a setting").name, "memory_limit");
    }

    /// The sentence three callers in two crates share, with the pin's blank line in the middle.
    #[test]
    fn an_unknown_setting_is_named_and_then_the_known_ones_are_listed() {
        let message = unknown_setting("nope");
        assert!(
            message.starts_with("unrecognized configuration parameter \"nope\"\n\nDid you mean: ")
        );
        for entry in SETTINGS {
            assert!(message.contains(&format!("\"{}\"", entry.name)), "{message}");
        }
    }
}
