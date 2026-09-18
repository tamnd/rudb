//! The pragmas that are a statement rather than a query, and what each one of them does.
//!
//! `PRAGMA version` is a query and rewrites to `SELECT * FROM pragma_version()`, which is what the
//! `pragma_*` table functions are for. `PRAGMA disable_optimizer` is not a query at all. It returns
//! one `BOOLEAN` column called `Success` with no rows in it, the way `SET` does, and what it is is a
//! `SET` somebody spelled as one word. The pin has nineteen of those among the thirty eight names
//! `duckdb_functions()` reports with a `function_type` of `pragma`, and this is the nineteen.
//!
//! # How the table was measured rather than read
//!
//! Each one was run against the pinned binary with `duckdb_settings()` snapshotted either side of
//! it, and the row that came back different is what the pragma writes. That is the only honest way
//! to fill this in: the names look like they say what they do and four of them do not. `PRAGMA
//! disable_print_progress_bar` writes `enable_progress_bar_print` and not `enable_print_progress_bar`,
//! which is not a setting at all. `PRAGMA enable_profiling` writes the word `query_tree` into a
//! `VARCHAR` rather than true into a boolean. `PRAGMA enable_profile` is the same statement under
//! another name. And `PRAGMA disable_profiling` puts the setting back to null rather than to the
//! empty string, which is why [`crate::UNSET`] exists.
//!
//! # Nine of them change nothing a query can see
//!
//! Those nine are not a gap in this table, they are what the pin does. `enable_object_cache`,
//! `disable_object_cache`, `enable_verification` and `disable_verification` are deprecated upstream
//! and print a warning saying they no longer have any effect. `enable_checkpoint_on_shutdown`,
//! `disable_checkpoint_on_shutdown`, `verify_parallelism`, `disable_verify_parallelism` and
//! `force_checkpoint` move a flag on the database that `duckdb_settings()` does not list and no
//! query can read. So the statement succeeds and writes nothing, which is what the pin does with
//! it, and the 692 corpus records charged to `disable_checkpoint_on_shutdown` alone are a file
//! saying something about checkpoints in its preamble and then going on to test something else.

/// One pragma that is a statement, and the setting it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PragmaEntry {
    /// The name as it is written after the word, which with no parentheses is the whole statement.
    pub name: &'static str,
    /// The setting it writes and the text it writes there, or `None` for one that writes nothing.
    pub writes: Option<(&'static str, &'static str)>,
}

/// The word `enable_profiling` takes, which is the pin's default output format and not a boolean.
const QUERY_TREE: &str = "query_tree";

/// Every pragma that is a statement, sorted by name.
pub static PRAGMAS: &[PragmaEntry] = &[
    PragmaEntry { name: "disable_checkpoint_on_shutdown", writes: None },
    PragmaEntry { name: "disable_object_cache", writes: None },
    PragmaEntry { name: "disable_optimizer", writes: Some(("enable_optimizer", "false")) },
    PragmaEntry {
        name: "disable_print_progress_bar",
        writes: Some(("enable_progress_bar_print", "false")),
    },
    PragmaEntry { name: "disable_profile", writes: Some(("enable_profiling", crate::UNSET)) },
    PragmaEntry { name: "disable_profiling", writes: Some(("enable_profiling", crate::UNSET)) },
    PragmaEntry { name: "disable_progress_bar", writes: Some(("enable_progress_bar", "false")) },
    PragmaEntry { name: "disable_verification", writes: None },
    PragmaEntry { name: "disable_verify_parallelism", writes: None },
    PragmaEntry { name: "enable_checkpoint_on_shutdown", writes: None },
    PragmaEntry { name: "enable_object_cache", writes: None },
    PragmaEntry { name: "enable_optimizer", writes: Some(("enable_optimizer", "true")) },
    PragmaEntry {
        name: "enable_print_progress_bar",
        writes: Some(("enable_progress_bar_print", "true")),
    },
    PragmaEntry { name: "enable_profile", writes: Some(("enable_profiling", QUERY_TREE)) },
    PragmaEntry { name: "enable_profiling", writes: Some(("enable_profiling", QUERY_TREE)) },
    PragmaEntry { name: "enable_progress_bar", writes: Some(("enable_progress_bar", "true")) },
    PragmaEntry { name: "enable_verification", writes: None },
    PragmaEntry { name: "force_checkpoint", writes: None },
    PragmaEntry { name: "verify_parallelism", writes: None },
];

/// The pragma of that name, however it was capitalized.
#[must_use]
pub fn pragma_named(name: &str) -> Option<&'static PragmaEntry> {
    PRAGMAS.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::{PRAGMAS, pragma_named};
    use crate::{SETTINGS, UNSET};

    #[test]
    fn the_table_is_the_nineteen_the_pin_has_and_is_sorted() {
        assert_eq!(PRAGMAS.len(), 19);
        let mut sorted: Vec<&str> = PRAGMAS.iter().map(|entry| entry.name).collect();
        let written = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, written, "the table is in the order the pin lists it, which is by name");
        assert_eq!(PRAGMAS.iter().filter(|entry| entry.writes.is_none()).count(), 9);
    }

    #[test]
    fn every_pragma_writes_a_setting_that_exists_and_a_value_of_its_type() {
        for entry in PRAGMAS {
            let Some((name, value)) = entry.writes else {
                continue;
            };
            let setting = SETTINGS
                .iter()
                .find(|it| it.name == name)
                .unwrap_or_else(|| panic!("{} writes {name}, which is not a setting", entry.name));
            match setting.input_type {
                "BOOLEAN" => assert!(
                    value == "true" || value == "false",
                    "{} writes {value} into a boolean",
                    entry.name
                ),
                "VARCHAR" => {}
                other => panic!("{} writes a {other}, which nothing here does yet", entry.name),
            }
        }
    }

    #[test]
    fn the_pair_that_turns_profiling_off_puts_it_back_to_nothing() {
        // Not the empty string. The pin reports null for `enable_profiling` on a fresh connection
        // and reports null again after `PRAGMA disable_profiling`, and a client that reads the
        // column gets a null either way.
        assert_eq!(pragma_named("disable_profiling").unwrap().writes.unwrap().1, UNSET);
        assert_eq!(pragma_named("DISABLE_PROFILE").unwrap().writes.unwrap().1, UNSET);
        assert_eq!(pragma_named("enable_profiling").unwrap().writes.unwrap().1, "query_tree");
    }

    #[test]
    fn a_name_that_is_not_one_of_them_is_not_found() {
        assert!(pragma_named("enable_verify_parallelism").is_none());
        assert!(pragma_named("enable_logging").is_none());
        assert!(pragma_named("version").is_none());
    }
}
