//! The knobs `SET` turns.
//!
//! A setting is not a catalog entry. It is not named by a query, it has no schema, and the set of
//! them is fixed at compile time, so this is a match on a name rather than a map. [`Settings::NAMES`]
//! is that set, and it is nine names for seven settings because two have a second spelling.
//! `max_memory` is `memory_limit` and `worker_threads` is `threads`, both ways round, which is what the binary does and what a client that writes the other spelling expects.
//! [`canonical`] is the one place that mapping lives, so a name arriving through `SET`, through
//! `RESET` or through a read of the value all land on the same setting.
//!
//! The seam settings are the exception to the fixed set, and they are a separate set rather than
//! three more names. `SET seam.hash.table = 'unchained'` picks which implementation runs at one of
//! the twenty seven seams in `rudb_seam`, there are twenty seven of them plus the policy, and none
//! of them is a DuckDB setting, so putting them in [`Settings::NAMES`] would make `duckdb_settings()`
//! list twenty eight names the binary has never heard of. They go through the same [`Settings::apply`]
//! anyway, because a second door into the settings is a second place for a scope rule to be wrong.
//!
//! Every setting here is global, which is the scope DuckDB gives them. `SET LOCAL` is
//! refused with the sentence the binary prints, and `SET SESSION` is refused with the one it prints
//! for a global setting, which is a different sentence and says which of the two the writer got
//! wrong.
//!
//! `duckdb_settings()` and `current_setting()` read these back from SQL, and both do it through
//! [`Settings::session`] rather than by reaching in here, because neither the binder nor the
//! executor can see this file from where they are. The table is built at execution and the function
//! is folded at binding, so the session is read once per statement and handed to both.
//! [`crate::Database::setting`] is the Rust side of the same read.

use std::sync::RwLock;

use rudb_common::{DefaultNullOrder, Error, Memory, Result, Session, Value, human};
use rudb_parse::ast::Scope;
use rudb_pipeline::Pool;
use rudb_seam::SEAM_PREFIX;

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
    /// The IANA time-zone name used to turn instants into session-local calendar fields.
    time_zone: RwLock<String>,
    /// The operating-system time zone restored by `RESET TimeZone` and `SET TIME ZONE LOCAL`.
    default_time_zone: String,
    /// The direction used when an order item says neither ascending nor descending.
    default_order: RwLock<String>,
    /// The null placement mode used when an order item does not state one.
    default_null_order: RwLock<String>,
    /// Whether `/` binds to integer division instead of floating point division.
    integer_division: RwLock<bool>,
    /// Which implementation runs at each seam, as `SET seam.<name>` has left it.
    ///
    /// Held here rather than in [`Config`], because there are twenty seven of them and a `Config`
    /// is a value a program copies. The session settings are the middle of the three surfaces in
    /// `spec/17-milestones.md`: the process flag sets them by running a `SET` at startup, and a
    /// per query hint is this with the query's own pins laid over a copy.
    seams: RwLock<rudb_seam::Settings>,
}

impl Settings {
    /// Every setting name, in the order `duckdb_settings()` lists them.
    pub(crate) const NAMES: [&'static str; 9] = [
        "TimeZone",
        "default_null_order",
        "default_order",
        "disabled_optimizers",
        "integer_division",
        "max_memory",
        "memory_limit",
        "threads",
        "worker_threads",
    ];

    /// The settings a database opened with this configuration starts at.
    pub(crate) fn new(config: Config) -> Self {
        let default_time_zone = iana_time_zone::get_timezone()
            .ok()
            .filter(|zone| Session::knows_time_zone(zone))
            .unwrap_or_else(|| "UTC".to_string());
        Self {
            defaults: config,
            current: RwLock::new(config),
            disabled: RwLock::new(String::new()),
            time_zone: RwLock::new(default_time_zone.clone()),
            default_time_zone,
            default_order: RwLock::new("ASCENDING".to_string()),
            default_null_order: RwLock::new("NULLS_LAST".to_string()),
            integer_division: RwLock::new(false),
            seams: RwLock::new(rudb_seam::Settings::new()),
        }
    }

    /// The passes `SET disabled_optimizers` turned off, for building an optimizer context.
    pub(crate) fn disabled_optimizers(&self) -> String {
        self.disabled.read().unwrap_or_else(|held| held.into_inner()).clone()
    }

    /// The seam settings as the statements have left them.
    ///
    /// A copy, because a statement reads them once at plan time and a reference would be a lock
    /// held for the length of the query. Twenty seven seams is a small map and most sessions pin
    /// none of them, so the copy is a copy of nothing much.
    pub(crate) fn seams(&self) -> rudb_seam::Settings {
        self.seams.read().unwrap_or_else(|held| held.into_inner()).clone()
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
        pool: &Pool,
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
        if is_seam(name) {
            // `RESET seam.hash.table` is the same thing as setting it to `default`, which is the
            // word the seam settings already use for an unpinned seam, so there is one path
            // through rather than a reset that has to know what a pin is.
            let text = match value {
                None => "default".to_string(),
                Some(value) => text_of(value),
            };
            return self
                .seams
                .write()
                .unwrap_or_else(|held| held.into_inner())
                .set(name, text.trim());
        }
        if !Self::NAMES.iter().any(|known| known.eq_ignore_ascii_case(name)) {
            return Err(Error::catalog(rudb_functions::unknown_setting(name)));
        }
        match canonical(name) {
            "TimeZone" => {
                let zone = match value {
                    None => self.default_time_zone.clone(),
                    Some(value) => text_of(value),
                };
                if !Session::knows_time_zone(&zone) {
                    return Err(Error::not_implemented(format!("Unknown TimeZone '{zone}'!")));
                }
                *self.time_zone.write().unwrap_or_else(|held| held.into_inner()) = zone;
            }
            "default_order" => {
                let Some(value) = value else {
                    *self.default_order.write().unwrap_or_else(|held| held.into_inner()) =
                        "ASCENDING".to_string();
                    return Ok(());
                };
                let written = text_of(value);
                let normalized = match written.to_ascii_uppercase().as_str() {
                    "ASC" | "ASCENDING" => "ASC",
                    "DESC" | "DESCENDING" => "DESC",
                    _ => {
                        return Err(Error::invalid_input(format!(
                            "Unrecognized parameter for option DEFAULT_ORDER \"{written}\". Expected ASC or DESC."
                        )));
                    }
                };
                *self.default_order.write().unwrap_or_else(|held| held.into_inner()) =
                    normalized.to_string();
            }
            "default_null_order" => {
                let written = value.map_or("NULLS_LAST".to_string(), text_of);
                let normalized = match written.to_ascii_uppercase().replace(' ', "_").as_str() {
                    "FIRST" | "NULLS_FIRST" => "NULLS_FIRST",
                    "LAST" | "NULLS_LAST" => "NULLS_LAST",
                    "SQLITE" => "SQLITE",
                    "MYSQL" => "MYSQL",
                    "POSTGRES" | "POSTGRESQL" => "POSTGRES",
                    _ => {
                        return Err(Error::parser(format!(
                            "Unrecognized parameter for option NULL_ORDER \"{written}\", expected either NULLS FIRST, NULLS LAST, SQLite, MySQL or Postgres"
                        )));
                    }
                };
                *self.default_null_order.write().unwrap_or_else(|held| held.into_inner()) =
                    normalized.to_string();
            }
            "disabled_optimizers" => {
                let text = match value {
                    None => String::new(),
                    Some(value) => text_of(value),
                };
                // Checked here rather than when a query builds its context, because the statement
                // that named a pass nobody has is the statement that should fail. What is kept is
                // what was understood rather than what was written, which is what the binary reads
                // back and so what `SELECT current_setting('disabled_optimizers')` has to say.
                let tidy = rudb_opt::pass::Context::tidy(&text)?;
                *self.disabled.write().unwrap_or_else(|held| held.into_inner()) = tidy;
            }
            "integer_division" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                *self.integer_division.write().unwrap_or_else(|held| held.into_inner()) = enabled;
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
                // The config is what `SELECT current_setting('threads')` reads back and the pool is
                // what a query actually asks for a degree, so both move or the setting is a number
                // that nothing obeys.
                pool.resize(threads);
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
        if is_seam(name) {
            return self.seams().get(name).ok_or_else(|| {
                Error::catalog(format!("no seam called {name}, see rudb_strategies() for the list"))
            });
        }
        let config = self.config();
        match canonical(name) {
            "TimeZone" => {
                Ok(self.time_zone.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "default_order" => {
                Ok(self.default_order.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "default_null_order" => {
                Ok(self.default_null_order.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "disabled_optimizers" => Ok(self.disabled_optimizers()),
            "integer_division" => Ok(self
                .integer_division
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            // An unlimited budget prints as the word rather than as a number, because rudb's
            // default is no limit where DuckDB's is a fraction of the machine, and printing the
            // largest number a limit could be would be describing a limit that is not there.
            "memory_limit" => Ok(config.memory_limit().map_or("unlimited".to_string(), human)),
            "threads" => Ok(config.threads().to_string()),
            _ => Err(Error::catalog(rudb_functions::unknown_setting(name))),
        }
    }

    /// Every setting and its value, for the table that lists them and the function that reads one.
    ///
    /// Built once per statement rather than held, because there are nine names and the alternative
    /// is a second copy of the settings that has to be kept in step with this one. An alias reports
    /// the same value as the name it resolves to, which is the same thing reading either spelling
    /// back gives, and it is what the binary returns for both halves of each pair.
    ///
    /// The two locks are taken once each here rather than once per name through [`Settings::value`],
    /// because every statement pays for this now that `current_setting()` can appear in any of them.
    /// Seven settings and nine names means the loop below would otherwise take several locks.
    pub(crate) fn session(&self) -> Session {
        let config = self.config();
        let disabled = self.disabled_optimizers();
        let memory = config.memory_limit().map_or_else(|| "unlimited".to_string(), human);
        let threads = config.threads().to_string();
        let time_zone = self.time_zone.read().unwrap_or_else(|held| held.into_inner()).clone();
        let default_order =
            self.default_order.read().unwrap_or_else(|held| held.into_inner()).clone();
        let default_null_order =
            self.default_null_order.read().unwrap_or_else(|held| held.into_inner()).clone();
        let integer_division =
            *self.integer_division.read().unwrap_or_else(|held| held.into_inner());
        let mut session = Session::new();
        session.set_time_zone(&time_zone);
        session.set_default_descending(default_order == "DESC");
        session.set_default_null_order(match default_null_order.as_str() {
            "NULLS_FIRST" => DefaultNullOrder::First,
            "SQLITE" | "MYSQL" => DefaultNullOrder::Sqlite,
            "POSTGRES" => DefaultNullOrder::Postgres,
            _ => DefaultNullOrder::Last,
        });
        session.set_integer_division(integer_division);
        for name in Self::NAMES {
            session.set(
                name,
                match canonical(name) {
                    "TimeZone" => time_zone.clone(),
                    "default_order" => default_order.clone(),
                    "default_null_order" => default_null_order.clone(),
                    "disabled_optimizers" => disabled.clone(),
                    "integer_division" => integer_division.to_string(),
                    "memory_limit" => memory.clone(),
                    "threads" => threads.clone(),
                    other => unreachable!("{other} is not one of NAMES"),
                },
            );
        }
        session
    }

    fn replace(&self, config: Config) {
        *self.current.write().unwrap_or_else(|held| held.into_inner()) = config;
    }
}

/// The setting a name means, which is itself for every name but the two aliases.
///
/// DuckDB puts the alias list on `max_memory` and `worker_threads` and leaves it empty on
/// `memory_limit` and `threads`, so by its own table the second of each pair is the canonical one.
/// That is the way round it is here too, because `memory_limit` and `threads` are the names the
/// documentation uses and the names everything else in rudb already spells.
fn canonical(name: &str) -> &str {
    if name.eq_ignore_ascii_case("timezone") {
        return "TimeZone";
    }
    if name.eq_ignore_ascii_case("default_order") {
        return "default_order";
    }
    if name.eq_ignore_ascii_case("default_null_order") {
        return "default_null_order";
    }
    match name {
        "max_memory" => "memory_limit",
        "worker_threads" => "threads",
        other => other,
    }
}

/// Whether this name is a seam rather than one of the settings DuckDB has.
///
/// A name with the `seam.` prefix is one whatever follows the prefix is, so that a mistyped seam
/// gets the error naming the seams rather than the one naming the DuckDB settings. Without
/// the prefix it has to be a name `rudb_seam` knows, which is where the three spellings of a seam
/// name are decided.
///
/// A DuckDB setting wins, which matters for exactly nothing today and costs one comparison. The
/// day DuckDB adds a setting whose name collides with a seam of ours, the compatible answer is the
/// one that wins and the prefixed spelling is still there for the other one.
fn is_seam(name: &str) -> bool {
    if Settings::NAMES.contains(&name) {
        return false;
    }
    name.starts_with(SEAM_PREFIX) || rudb_seam::seam_named(name).is_some()
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

/// A value cast to a boolean by the same spellings DuckDB's boolean cast accepts.
fn boolean_of(value: &Value) -> Result<bool> {
    let converted = match value {
        Value::Boolean(value) => Some(*value),
        Value::TinyInt(value) => Some(*value != 0),
        Value::SmallInt(value) => Some(*value != 0),
        Value::Integer(value) => Some(*value != 0),
        Value::BigInt(value) => Some(*value != 0),
        Value::HugeInt(value) => Some(*value != 0),
        Value::UTinyInt(value) => Some(*value != 0),
        Value::USmallInt(value) => Some(*value != 0),
        Value::UInteger(value) => Some(*value != 0),
        Value::UBigInt(value) => Some(*value != 0),
        Value::Varchar(text) => match text.to_ascii_lowercase().as_str() {
            "true" | "t" | "yes" | "y" | "1" => Some(true),
            "false" | "f" | "no" | "n" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    };
    converted.ok_or_else(|| {
        Error::invalid_input(format!(
            "Failed to cast value: Could not convert string '{}' to BOOL",
            text_of(value)
        ))
    })
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
    use rudb_pipeline::Pool;

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
        settings
            .apply(&memory, &Pool::default(), "memory_limit", Scope::Unwritten, Some(&value))
            .expect("a size");
        assert_eq!(memory.limit(), Some(1 << 30));
        assert_eq!(settings.value("memory_limit").expect("a setting"), "1.0 GiB");
        settings
            .apply(&memory, &Pool::default(), "memory_limit", Scope::Unwritten, None)
            .expect("a reset");
        assert_eq!(memory.limit(), None, "reset goes back to what the database was opened with");
    }

    #[test]
    fn a_pass_that_is_not_a_pass_is_refused_by_the_statement_that_named_it() {
        let (settings, memory) = settings();
        let value = Value::Varchar("bogus".into());
        let error = settings
            .apply(&memory, &Pool::default(), "disabled_optimizers", Scope::Unwritten, Some(&value))
            .expect_err("not a pass");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert_eq!(settings.disabled_optimizers(), "", "a refused set changed nothing");
    }

    #[test]
    fn a_name_that_is_not_a_setting_says_so_with_the_names_there_are() {
        let (settings, memory) = settings();
        let error = settings
            .apply(&memory, &Pool::default(), "bogus", Scope::Unwritten, None)
            .expect_err("not a setting");
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
            .apply(&memory, &Pool::default(), "threads", Scope::Local, Some(&value))
            .expect_err("no local scope");
        assert_eq!(error.message(), "SET LOCAL is not implemented.");
        let error = settings
            .apply(&memory, &Pool::default(), "threads", Scope::Session, Some(&value))
            .expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be set locally");
        // The word changes with the statement, because a writer who wrote `RESET` should not read a
        // sentence about `SET`.
        let error = settings
            .apply(&memory, &Pool::default(), "threads", Scope::Local, None)
            .expect_err("no local scope");
        assert_eq!(error.message(), "RESET LOCAL is not implemented.");
        let error = settings
            .apply(&memory, &Pool::default(), "threads", Scope::Session, None)
            .expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be reset locally");
    }

    #[test]
    fn a_thread_count_is_a_whole_number_of_at_least_one() {
        let (settings, memory) = settings();
        let pool = Pool::new(1);
        settings
            .apply(&memory, &pool, "threads", Scope::Global, Some(&Value::BigInt(4)))
            .expect("four threads");
        assert_eq!(settings.value("threads").expect("a setting"), "4");
        assert_eq!(pool.threads(), 4, "the setting reached the thing that hands out threads");
        let error = settings
            .apply(&memory, &Pool::default(), "threads", Scope::Global, Some(&Value::BigInt(0)))
            .expect_err("no threads at all");
        assert_eq!(error.message(), "Must have at least 1 thread!");
        let text = Value::Varchar("abc".into());
        let error = settings
            .apply(&memory, &Pool::default(), "threads", Scope::Global, Some(&text))
            .expect_err("not a number");
        assert_eq!(
            error.message(),
            "Failed to cast value: Could not convert string 'abc' to INT64"
        );
    }
}
