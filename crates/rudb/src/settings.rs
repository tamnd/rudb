//! The knobs `SET` turns.
//!
//! A setting is not a catalog entry. It is not named by a query, it has no schema, and the set of
//! them is fixed at compile time, so this is a match on a name rather than a map. [`Settings::NAMES`]
//! is that set, and it is three because [`crate::Config`] holds three things a program can choose
//! and a fourth that DuckDB has no setting for.
//!
//! Every setting here is global, which is the scope DuckDB gives all three of them. `SET LOCAL` is
//! refused with the sentence the binary prints, and `SET SESSION` is refused with the one it prints
//! for a global setting, which is a different sentence and says which of the two the writer got
//! wrong.
//!
//! What is not here yet is `current_setting()` and `duckdb_settings()`, which are the two ways SQL
//! reads a setting back rather than writing one. [`crate::Database::setting`] is the Rust side of
//! that read and the SQL side is a scalar function over engine state, which is a shape no function
//! in rudb has yet.

use std::sync::RwLock;

use rudb_common::{Error, Memory, Result, Value, human};
use rudb_parse::ast::Scope;

use crate::config::{Config, parse_size};

/// The settings of one database, and what `RESET` puts them back to.
#[derive(Debug)]
pub(crate) struct Settings {
    /// What the database was opened with, which is what `RESET` restores.
    defaults: Config,
    current: RwLock<Config>,
    /// The passes turned off, as written, empty for none.
    ///
    /// Kept as the text rather than as an `rudb_opt::Context`, because the text is what `RESET`
    /// compares against and what a read of the setting has to hand back. It is validated when it is
    /// set, so the context it builds later cannot fail.
    disabled: RwLock<String>,
}

impl Settings {
    /// Every setting name, in the order `duckdb_settings()` would list them.
    pub(crate) const NAMES: [&'static str; 3] = ["disabled_optimizers", "memory_limit", "threads"];

    /// The settings a database opened with this configuration starts at.
    pub(crate) fn new(config: Config) -> Self {
        Self {
            defaults: config,
            current: RwLock::new(config),
            disabled: RwLock::new(String::new()),
        }
    }

    /// The passes `SET disabled_optimizers` turned off, for building an optimizer context.
    pub(crate) fn disabled_optimizers(&self) -> String {
        self.disabled.read().unwrap_or_else(|held| held.into_inner()).clone()
    }

    /// The configuration as the statements have left it.
    pub(crate) fn config(&self) -> Config {
        *self.current.read().unwrap_or_else(|held| held.into_inner())
    }

    /// What the database was opened with, which is what `RESET` restores.
    pub(crate) fn defaults(&self) -> Config {
        self.defaults
    }

    /// Applies a `SET`, or a `RESET` when there is no value.
    ///
    /// # Errors
    ///
    /// For a name that is not a setting, for a scope this database does not have, and for a value
    /// the setting cannot take.
    pub(crate) fn apply(
        &self,
        memory: &Memory,
        name: &str,
        scope: Scope,
        value: Option<&Value>,
    ) -> Result<()> {
        let word = if value.is_some() { "SET" } else { "RESET" };
        let verb = if value.is_some() { "set" } else { "reset" };
        match scope {
            Scope::Local => {
                return Err(Error::not_implemented(format!("{word} LOCAL is not implemented.")));
            }
            // Every setting here is global, so naming the session is naming a copy that does not
            // exist. The day one of them is per connection this becomes a question about the name.
            Scope::Session => {
                return Err(Error::catalog(format!("option \"{name}\" cannot be {verb} locally")));
            }
            Scope::Global | Scope::Unwritten => {}
        }
        if !Self::NAMES.contains(&name) {
            let known: Vec<String> =
                Self::NAMES.iter().map(|known| format!("\"{known}\"")).collect();
            return Err(Error::catalog(format!(
                "unrecognized configuration parameter \"{name}\"\n\nDid you mean: {}",
                known.join(", ")
            )));
        }
        match name {
            "disabled_optimizers" => {
                let text = match value {
                    None => String::new(),
                    Some(value) => text_of(value),
                };
                // Checked here rather than when a query builds its context, because the statement
                // that named a pass nobody has is the statement that should fail.
                rudb_opt::pass::Context::without(&text)?;
                *self.disabled.write().unwrap_or_else(|held| held.into_inner()) = text;
            }
            "memory_limit" => {
                let limit = match value {
                    None => self.defaults.memory_limit(),
                    Some(value) => bytes_of(&text_of(value))?,
                };
                let config = self.config();
                self.replace(match limit {
                    Some(bytes) => config.with_memory_limit(bytes),
                    None => config.with_no_memory_limit(),
                });
                memory.set_limit(limit);
            }
            "threads" => {
                let threads = match value {
                    None => self.defaults.threads(),
                    Some(value) => threads_of(value)?,
                };
                self.replace(self.config().with_threads(threads)?);
            }
            _ => unreachable!("the name was one of NAMES a moment ago"),
        }
        Ok(())
    }

    /// One setting, in the spelling DuckDB prints for it.
    ///
    /// # Errors
    ///
    /// For a name that is not a setting.
    pub(crate) fn value(&self, name: &str) -> Result<String> {
        let config = self.config();
        match name {
            "disabled_optimizers" => Ok(self.disabled_optimizers()),
            // An unlimited budget prints as the word rather than as a number, because rudb's
            // default is no limit where DuckDB's is a fraction of the machine, and printing the
            // largest number a limit could be would be describing a limit that is not there.
            "memory_limit" => Ok(config.memory_limit().map_or("unlimited".to_string(), human)),
            "threads" => Ok(config.threads().to_string()),
            _ => {
                let known: Vec<String> =
                    Self::NAMES.iter().map(|known| format!("\"{known}\"")).collect();
                Err(Error::catalog(format!(
                    "unrecognized configuration parameter \"{name}\"\n\nDid you mean: {}",
                    known.join(", ")
                )))
            }
        }
    }

    fn replace(&self, config: Config) {
        *self.current.write().unwrap_or_else(|held| held.into_inner()) = config;
    }
}

/// A value as the text a setting reads.
///
/// `SET disabled_optimizers = 5` is a number where a name was wanted, and DuckDB casts it to a
/// string and then complains that it is not an optimizer, which is a better sentence than one about
/// types because the writer's mistake is the name and not the quotes.
fn text_of(value: &Value) -> String {
    match value {
        Value::Varchar(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The thread count a value names.
fn threads_of(value: &Value) -> Result<usize> {
    let count = match value {
        Value::TinyInt(count) => i128::from(*count),
        Value::SmallInt(count) => i128::from(*count),
        Value::Integer(count) => i128::from(*count),
        Value::BigInt(count) => i128::from(*count),
        Value::HugeInt(count) => *count,
        Value::Varchar(text) => text.trim().parse::<i128>().map_err(|_| {
            Error::invalid_input(format!(
                "Failed to cast value: Could not convert string '{text}' to INT64"
            ))
        })?,
        other => {
            return Err(Error::invalid_input(format!(
                "Failed to cast value: Could not convert {} to INT64",
                other.logical_type()
            )));
        }
    };
    // A `Syntax Error` for a value that is the right type and the wrong number is not the class
    // anybody would pick, and it is the class the binary prints, so it is the class here.
    usize::try_from(count)
        .ok()
        .filter(|count| *count >= 1)
        .ok_or_else(|| Error::syntax("Must have at least 1 thread!"))
}

/// The byte count a memory size names, or `None` for no limit.
///
/// [`parse_size`] with the one difference `SET memory_limit` has from the rest of rudb: the unit is
/// required, because DuckDB requires it, and `-1` is how you say no limit at all. Everything else is
/// the same function, so the statement and the `Config` builder cannot drift apart on what `1GB`
/// means.
///
/// # Errors
///
/// [`rudb_common::ErrorCode::Parser`], with the sentence the binary prints, for a number with no
/// unit, a unit that is not one of the nine and text that is not a number.
fn bytes_of(text: &str) -> Result<Option<u64>> {
    let trimmed = text.trim();
    if trimmed == "-1" {
        return Ok(None);
    }
    if !trimmed.contains(|character: char| character.is_ascii_alphabetic()) {
        return Err(Error::parser(
            "Unknown unit for memory: '' (expected: KB, MB, GB, TB for 1000^i units or KiB, MiB, GiB, TiB for 1024^i units)",
        ));
    }
    parse_size(trimmed).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{Settings, bytes_of};
    use crate::config::Config;
    use rudb_common::{Memory, Value};
    use rudb_parse::ast::Scope;

    /// A settings object over a database opened with no limit at all.
    ///
    /// `Config::new()` has one now, eighty percent of the machine, so a test about what `RESET`
    /// goes back to has to say which "back" it means. Here it is no limit, because that is the
    /// state where a wrong reset is visible rather than being a different large number.
    fn settings() -> (Settings, Memory) {
        (Settings::new(Config::new().with_no_memory_limit()), Memory::unlimited())
    }

    #[test]
    fn a_size_is_the_number_times_the_unit_and_the_unit_is_required() {
        assert_eq!(bytes_of("1GB").expect("a size"), Some(1_000_000_000));
        assert_eq!(bytes_of("1GiB").expect("a size"), Some(1 << 30));
        assert_eq!(bytes_of("1.5gb").expect("a lower case size"), Some(1_500_000_000));
        assert_eq!(bytes_of("100 MB").expect("a spaced size"), Some(100_000_000));
        assert_eq!(bytes_of("-1").expect("the way to say no limit"), None);
        let error = bytes_of("0").expect_err("a number with no unit");
        assert!(error.message().starts_with("Unknown unit for memory: ''"), "{}", error.message());
        let error = bytes_of("abc").expect_err("not a number at all");
        assert_eq!(error.message(), "Memory must have a number (e.g. 1GB)");
    }

    #[test]
    fn setting_the_memory_limit_moves_the_budget_every_query_is_held_to() {
        let (settings, memory) = settings();
        assert_eq!(memory.limit(), None);
        let value = Value::Varchar("1GiB".into());
        settings.apply(&memory, "memory_limit", Scope::Unwritten, Some(&value)).expect("a size");
        assert_eq!(memory.limit(), Some(1 << 30));
        assert_eq!(settings.value("memory_limit").expect("a setting"), "1.0 GiB");
        settings.apply(&memory, "memory_limit", Scope::Unwritten, None).expect("a reset");
        assert_eq!(memory.limit(), None, "reset goes back to what the database was opened with");
    }

    #[test]
    fn a_pass_that_is_not_a_pass_is_refused_by_the_statement_that_named_it() {
        let (settings, memory) = settings();
        let value = Value::Varchar("bogus".into());
        let error = settings
            .apply(&memory, "disabled_optimizers", Scope::Unwritten, Some(&value))
            .expect_err("not a pass");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert_eq!(settings.disabled_optimizers(), "", "a refused set changed nothing");
    }

    #[test]
    fn a_name_that_is_not_a_setting_says_so_with_the_names_there_are() {
        let (settings, memory) = settings();
        let error =
            settings.apply(&memory, "bogus", Scope::Unwritten, None).expect_err("not a setting");
        assert_eq!(error.code().duckdb_name(), "Catalog Error");
        assert!(
            error.message().starts_with("unrecognized configuration parameter \"bogus\""),
            "{}",
            error.message()
        );
        assert!(error.message().contains("\"memory_limit\""), "{}", error.message());
    }

    #[test]
    fn the_two_scopes_this_database_does_not_have_are_two_different_sentences() {
        let (settings, memory) = settings();
        let value = Value::BigInt(2);
        let error = settings
            .apply(&memory, "threads", Scope::Local, Some(&value))
            .expect_err("no local scope");
        assert_eq!(error.message(), "SET LOCAL is not implemented.");
        let error = settings
            .apply(&memory, "threads", Scope::Session, Some(&value))
            .expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be set locally");
        // The word changes with the statement, because a writer who wrote `RESET` should not read a
        // sentence about `SET`.
        let error =
            settings.apply(&memory, "threads", Scope::Local, None).expect_err("no local scope");
        assert_eq!(error.message(), "RESET LOCAL is not implemented.");
        let error =
            settings.apply(&memory, "threads", Scope::Session, None).expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be reset locally");
    }

    #[test]
    fn a_thread_count_is_a_whole_number_of_at_least_one() {
        let (settings, memory) = settings();
        settings
            .apply(&memory, "threads", Scope::Global, Some(&Value::BigInt(4)))
            .expect("four threads");
        assert_eq!(settings.value("threads").expect("a setting"), "4");
        let error = settings
            .apply(&memory, "threads", Scope::Global, Some(&Value::BigInt(0)))
            .expect_err("no threads at all");
        assert_eq!(error.message(), "Must have at least 1 thread!");
        let text = Value::Varchar("abc".into());
        let error = settings
            .apply(&memory, "threads", Scope::Global, Some(&text))
            .expect_err("not a number");
        assert_eq!(
            error.message(),
            "Failed to cast value: Could not convert string 'abc' to INT64"
        );
    }
}
