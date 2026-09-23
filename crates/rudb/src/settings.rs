//! The knobs `SET` turns.
//!
//! A setting is not a catalog entry.
//! It is not named by a query, it has no schema, and the set of them is fixed at compile time, so which names exist lives in `rudb_functions::settingcatalog` and this is a match on a name rather than a map.
//! There are a hundred and ninety two names for a hundred and eighty five settings, because seven of them have a second spelling.
//! `max_memory` is `memory_limit` and `worker_threads` is `threads`, both ways round, which is what the binary does and what a client that writes the other spelling expects.
//! [`canonical`] is the one place that mapping lives, so a name arriving through `SET`, through
//! `RESET` or through a read of the value all land on the same setting.
//!
//! Twenty three of the names are settings the engine reads, and each of those has a field here.
//! The other hundred and sixty nine name parts of DuckDB rudb has no counterpart for, they are read
//! and written through their name and nothing else, and [`Settings::carried`] is where they live.
//! `rudb_functions::Behaviour` is which of the two a name is and, for a carried one, whether taking
//! any value but its default would be a lie. That split is argued for in the catalog's own module
//! documentation, since it is a statement about the table rather than about this file.
//!
//! The seam settings are the exception to the fixed set, and they are a separate set rather than
//! three more names. `SET seam.hash.table = 'unchained'` picks which implementation runs at one of
//! the twenty seven seams in `rudb_seam`, there are twenty seven of them plus the policy, and none
//! of them is a DuckDB setting, so putting them in the settings table would make `duckdb_settings()`
//! list twenty eight names the binary has never heard of. They go through the same [`Settings::apply`]
//! anyway, because a second door into the settings is a second place for a scope rule to be wrong.
//!
//! The rule switches of `rudb_common::rules` are the other exception and they work the same way.
//! `SET stats.presize = false` turns off one optimization so that its worth can be measured on its
//! own, and `SET statistics = off` turns off all of them at once, which is the ablation
//! `spec/stats/09-measurement.md` section 9.3 runs on every commit. `SET graph_sections = off` is
//! the same idea for the stored graph sections. None of the eleven is a DuckDB setting either, so none
//! of them is in the settings catalog and `duckdb_settings()` does not list them.
//!
//! `cluster_by` is an exception of a third kind, and the interesting one. It is not a DuckDB
//! setting either, but unlike the seams and the rules it keeps nothing here at all. What it writes
//! is the clustering on the tables the text names, and what it reads is those same declarations
//! built back into text by [`clustering`]. A copy in the session would be a second answer that goes
//! wrong the moment a database is opened on a file whose tables already carry declarations nobody
//! in this session set. The declaration arrives as a setting rather than as a `CLUSTER BY` clause
//! because the grammar is DuckDB's and has no such clause, which is the same reason
//! `SET graph_links` looks the way it does.
//!
//! Every setting the engine reads is global, which is the scope DuckDB gives it, and for those
//! `SET LOCAL` is refused with the sentence the binary prints and `SET SESSION` with the one it
//! prints for a global setting, which is a different sentence and says which of the two the writer
//! got wrong. Fifteen of the carried names are per connection on the pin, and for those all three
//! spellings are taken and land in the same place, because rudb has one connection's worth of state
//! and nothing reads the value anyway.
//!
//! `duckdb_settings()` and `current_setting()` read these back from SQL, and both do it through
//! [`Settings::session`] rather than by reaching in here, because neither the binder nor the
//! executor can see this file from where they are. The table is built at execution and the function
//! is folded at binding, so the session is read once per statement and handed to both.
//! [`crate::Database::setting`] is the Rust side of the same read.

use std::collections::BTreeMap;
use std::sync::RwLock;

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{
    Clustering, Declared, DefaultNullOrder, Error, IdentifierCase, Memory, Result, Rules, Session,
    ShowBehavior, Value, human, looks_like_rule, parse_clustering, rule_names,
};
use rudb_functions::{Behaviour, LOCAL, SETTINGS, SettingEntry};
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
    /// The installed parser dialect selected for new statements.
    current_dialect: RwLock<String>,
    /// Whether an extension may replace the parser selected for new statements.
    allow_parser_override_extension: RwLock<String>,
    /// The selected SQL compatibility mode as written, or `NONE` for ordinary DuckDB rules.
    dialect_compatibility_mode: RwLock<String>,
    /// The null placement mode used when an order item does not state one.
    default_null_order: RwLock<String>,
    /// Whether casts from local timestamps to zoned timestamps are refused.
    disable_timestamptz_casts: RwLock<bool>,
    /// Whether errors are returned as structured JSON.
    errors_as_json: RwLock<bool>,
    /// Whether floating division and remainder use IEEE answers for zero divisors.
    ieee_floating_point_ops: RwLock<bool>,
    /// Whether `/` binds to integer division instead of floating point division.
    integer_division: RwLock<bool>,
    /// How unquoted identifiers are folded before binding.
    preserve_identifier_case: RwLock<String>,
    /// Whether a zero divisor that normally raises returns null.
    null_on_division_by_zero: RwLock<bool>,
    /// Whether an `ORDER BY` may name a non-integer literal that cannot affect the order.
    order_by_non_integer_literal: RwLock<bool>,
    /// Whether regex match operators require the entire string to match.
    regex_match_operator_semantics: RwLock<String>,
    /// Whether a scalar query producing several rows raises an error.
    scalar_subquery_error_on_multiple_rows: RwLock<bool>,
    /// How a bare name following `SHOW` is resolved.
    show_behavior: RwLock<String>,
    /// Whether warnings are promoted to errors.
    warnings_as_errors: RwLock<bool>,
    /// The settings rudb takes and does not act on, as the statements have left them.
    ///
    /// Every setting above has a field of its own, because the engine reads it and a field is where
    /// a thing that is read belongs. These are the other hundred and sixty nine, they are written
    /// and read back through their name and nothing else looks at them, so one map holds the lot. A
    /// name that is not in the map is at its default, which is why `RESET` is a removal here.
    carried: RwLock<BTreeMap<&'static str, String>>,
    /// Which implementation runs at each seam, as `SET seam.<name>` has left it.
    ///
    /// Held here rather than in [`Config`], because there are twenty seven of them and a `Config`
    /// is a value a program copies. The session settings are the middle of the three surfaces in
    /// `spec/17-milestones.md`: the process flag sets them by running a `SET` at startup, and a
    /// per query hint is this with the query's own pins laid over a copy.
    seams: RwLock<rudb_seam::Settings>,
    /// Which optimization rules may fire, as `SET stats.<name>` and `SET graph.sections` have left
    /// them.
    ///
    /// The same exception the seams are, for the same reason, and `rudb_common::rules` says why the
    /// names are not in the settings catalog. Every statistics rule starts on, so a database that
    /// never mentions one of these behaves as it did before any of them existed, and the graph
    /// sections start off because they are a new path rather than a better estimate.
    rules: RwLock<Rules>,
    /// The relationships `SET graph_links` declared, as they were written.
    ///
    /// The third exception, and the one with the most to say for itself. Section 2.5 of
    /// spec/graph/02-the-data-model.md gives three ways a relationship can be declared, and this is
    /// the second, which is the one TPC-H needs: the tables arrive from Parquet, Parquet has no
    /// foreign keys, so there is no constraint for the first path to read and nothing yet for the
    /// third to have inferred. It is not a DuckDB setting and so is not in the settings catalog,
    /// for the reason the seams and the rules are not.
    ///
    /// Kept as the text for the reason the disabled passes are: the text is what `RESET` compares
    /// against and what a read of the setting has to return, and it was parsed once when it was
    /// set, so nothing downstream has a parse that can fail.
    links: RwLock<String>,
    /// The two numbers the link join rule of `spec/graph/06-the-optimizer.md` section 6.4 is
    /// decided by.
    ///
    /// Settings for the reason that section asks for them to be: neither one can be read off the
    /// machine the query is running on. The first is how much of a parent has to fit for its hash
    /// table to stay in the last level cache, which is a property of the processor, and the second
    /// is the projected width at which the build stops paying for itself, which is a crossover a
    /// measurement moves. Both start where `rudb_opt` has them.
    sizes: RwLock<rudb_opt::link::Sizes>,
}

impl Settings {
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
            current_dialect: RwLock::new("duckdb".to_string()),
            allow_parser_override_extension: RwLock::new("DEFAULT".to_string()),
            dialect_compatibility_mode: RwLock::new("NONE".to_string()),
            default_null_order: RwLock::new("NULLS_LAST".to_string()),
            disable_timestamptz_casts: RwLock::new(false),
            errors_as_json: RwLock::new(false),
            ieee_floating_point_ops: RwLock::new(true),
            integer_division: RwLock::new(false),
            preserve_identifier_case: RwLock::new("preserve_case".to_string()),
            null_on_division_by_zero: RwLock::new(false),
            order_by_non_integer_literal: RwLock::new(false),
            regex_match_operator_semantics: RwLock::new("partial".to_string()),
            scalar_subquery_error_on_multiple_rows: RwLock::new(true),
            show_behavior: RwLock::new("AUTO".to_string()),
            warnings_as_errors: RwLock::new(false),
            carried: RwLock::new(BTreeMap::new()),
            seams: RwLock::new(rudb_seam::Settings::new()),
            rules: RwLock::new(Rules::new()),
            links: RwLock::new(String::new()),
            sizes: RwLock::new(rudb_opt::link::Sizes::default()),
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

    /// The rules as the statements have left them.
    ///
    /// A copy for the same reason the seams are copied: a statement reads them once and a reference
    /// would be a lock held for the length of the query. This one is two bytes.
    pub(crate) fn rules(&self) -> Rules {
        *self.rules.read().unwrap_or_else(|held| held.into_inner())
    }

    /// The relationships declared so far, as they were written.
    pub(crate) fn links(&self) -> String {
        self.links.read().unwrap_or_else(|held| held.into_inner()).clone()
    }

    /// The two link join numbers as the statements have left them.
    pub(crate) fn sizes(&self) -> rudb_opt::link::Sizes {
        *self.sizes.read().unwrap_or_else(|held| held.into_inner())
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
        catalog: &mut Catalog,
        name: &str,
        scope: Scope,
        value: Option<&Value>,
    ) -> Result<()> {
        let word = if value.is_some() { "SET" } else { "RESET" };
        let verb = if value.is_some() { "set" } else { "reset" };
        // Every setting rudb reads is global, and so is every seam, so naming the session or a local
        // copy is naming something that does not exist. Fifteen of the names rudb carries and does
        // not read are per connection on the pin, and those take all three spellings and land in the
        // same place, because there is one connection's worth of state here and nothing reads the
        // value either way.
        let per_connection =
            rudb_functions::setting_named(name).is_some_and(|it| it.scope == LOCAL);
        match scope {
            Scope::Local if !per_connection => {
                return Err(Error::not_implemented(format!("{word} LOCAL is not implemented.")));
            }
            Scope::Session if !per_connection => {
                return Err(Error::catalog(format!("option \"{name}\" cannot be {verb} locally")));
            }
            _ => {}
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
        if is_links(name) {
            // Validated here and nowhere else. A declaration that does not parse is a mistake in a
            // statement somebody just typed, so it is refused where they can see it rather than
            // carried to a checkpoint that quietly builds nothing.
            let written = value.map_or_else(String::new, text_of);
            rudb_graph::parse_links(&written)?;
            *self.links.write().unwrap_or_else(|held| held.into_inner()) = written;
            return Ok(());
        }
        if is_clustering(name) {
            // Nothing is kept here. The declarations live on the tables, which is where a
            // checkpoint reads them and where an open puts the ones the file holds, so a copy in
            // the session would be a second answer that goes stale the moment a file is opened.
            // [`clustering`] is the read, and it builds the text back out of the catalog.
            return declare(catalog, &value.map_or_else(String::new, text_of));
        }
        if let Some(which) = graph_size(name) {
            let mut sizes = self.sizes.write().unwrap_or_else(|held| held.into_inner());
            let default = rudb_opt::link::Sizes::default();
            match (which, value) {
                (Size::Cache, None) => sizes.cache_bytes = default.cache_bytes,
                (Size::Cache, Some(value)) => sizes.cache_bytes = size_of(value, name)?,
                (Size::Narrow, None) => sizes.narrow_bytes = default.narrow_bytes,
                (Size::Narrow, Some(value)) => {
                    sizes.narrow_bytes = usize::try_from(size_of(value, name)?)
                        .map_err(|_| Error::invalid_input(format!("{name} is too large")))?;
                }
            }
            return Ok(());
        }
        if is_rule(name) {
            // `RESET stats.presize` puts the rule back where a fresh database has it, which is on
            // for the statistics rules and off for the graph sections.
            let mut rules = self.rules.write().unwrap_or_else(|held| held.into_inner());
            return match value {
                None => rules.reset_named(name),
                Some(value) => rules.set_named(name, switch_of(value)?),
            };
        }
        let Some(entry) = rudb_functions::setting_named(canonical(name)) else {
            return Err(Error::catalog(rudb_functions::unknown_setting(name)));
        };
        if entry.behaviour != Behaviour::Honoured {
            return self.carry(entry, value);
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
            "allow_parser_override_extension" => {
                let written = value.map_or("DEFAULT".to_string(), text_of);
                if !written.eq_ignore_ascii_case("default") {
                    return Err(Error::not_implemented(format!(
                        "Enum value: unrecognized value \"{written}\" for enum \"AllowParserOverride\"\n\nCandidates: \"DEFAULT\""
                    )));
                }
                *self
                    .allow_parser_override_extension
                    .write()
                    .unwrap_or_else(|held| held.into_inner()) = "DEFAULT".to_string();
            }
            "current_dialect" => {
                let written = value.map_or("duckdb".to_string(), text_of);
                if rudb_parse::dialect::dialect_named(&written).is_none() {
                    return Err(Error::invalid_input(format!(
                        "Dialect \"{written}\" is not installed"
                    )));
                }
                *self.current_dialect.write().unwrap_or_else(|held| held.into_inner()) = written;
            }
            "dialect_compatibility_mode" => {
                let written = value.map_or("NONE".to_string(), text_of);
                if !written.eq_ignore_ascii_case("none") && !written.eq_ignore_ascii_case("spark") {
                    return Err(Error::not_implemented(format!(
                        "Enum value: unrecognized value \"{written}\" for enum \"DialectCompatibilityMode\"\n\nCandidates: \"NONE\""
                    )));
                }
                *self.dialect_compatibility_mode.write().unwrap_or_else(|held| held.into_inner()) =
                    written;
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
            "disable_timestamptz_casts" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                *self.disable_timestamptz_casts.write().unwrap_or_else(|held| held.into_inner()) =
                    enabled;
            }
            "errors_as_json" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                *self.errors_as_json.write().unwrap_or_else(|held| held.into_inner()) = enabled;
            }
            "ieee_floating_point_ops" => {
                let enabled = value.map_or(Ok(true), boolean_of)?;
                *self.ieee_floating_point_ops.write().unwrap_or_else(|held| held.into_inner()) =
                    enabled;
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
            "null_on_division_by_zero" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                *self.null_on_division_by_zero.write().unwrap_or_else(|held| held.into_inner()) =
                    enabled;
            }
            "order_by_non_integer_literal" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                *self
                    .order_by_non_integer_literal
                    .write()
                    .unwrap_or_else(|held| held.into_inner()) = enabled;
            }
            "preserve_identifier_case" => {
                if matches!(value, Some(Value::Null)) {
                    return Err(Error::invalid_input(
                        "preserve_identifier_case setting cannot be NULL",
                    ));
                }
                let written = match value {
                    None => "preserve_case".to_string(),
                    Some(value) => match boolean_of(value) {
                        Ok(true) => "preserve_case".to_string(),
                        Ok(false) => "lowercase".to_string(),
                        Err(_) => text_of(value),
                    },
                };
                let normalized = match written.to_ascii_lowercase().as_str() {
                    "preserve_case" => "preserve_case",
                    "lowercase" => "lowercase",
                    "uppercase" => "uppercase",
                    _ => {
                        return Err(Error::invalid_input(format!(
                            "Unrecognized parameter for option preserve_identifier_case \"{written}\", expected one of: preserve_case, lowercase, uppercase"
                        )));
                    }
                };
                *self.preserve_identifier_case.write().unwrap_or_else(|held| held.into_inner()) =
                    normalized.to_string();
            }
            "regex_match_operator_semantics" => {
                let written = value.map_or("partial".to_string(), text_of);
                if !written.eq_ignore_ascii_case("partial") && !written.eq_ignore_ascii_case("full")
                {
                    let candidate = if self
                        .regex_match_operator_semantics
                        .read()
                        .unwrap_or_else(|held| held.into_inner())
                        .eq_ignore_ascii_case("partial")
                    {
                        "FULL"
                    } else {
                        "PARTIAL"
                    };
                    return Err(Error::not_implemented(format!(
                        "Enum value: unrecognized value \"{written}\" for enum \"RegexMatchOperatorSemantics\"\n\nCandidates: \"{candidate}\""
                    )));
                }
                *self
                    .regex_match_operator_semantics
                    .write()
                    .unwrap_or_else(|held| held.into_inner()) = written;
            }
            "scalar_subquery_error_on_multiple_rows" => {
                let enabled = value.map_or(Ok(true), boolean_of)?;
                *self
                    .scalar_subquery_error_on_multiple_rows
                    .write()
                    .unwrap_or_else(|held| held.into_inner()) = enabled;
            }
            "show_behavior" => {
                let written = value.map_or("AUTO".to_string(), text_of);
                match written.to_ascii_uppercase().as_str() {
                    "AUTO" | "SETTING" | "TABLE" => {}
                    _ => {
                        return Err(Error::not_implemented(format!(
                            "Enum value: unrecognized value \"{written}\" for enum \"ShowBehaviorType\"\n\nCandidates: \"AUTO\""
                        )));
                    }
                }
                *self.show_behavior.write().unwrap_or_else(|held| held.into_inner()) = written;
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
            "warnings_as_errors" => {
                let enabled = value.map_or(Ok(false), boolean_of)?;
                if enabled {
                    return Err(Error::settings(
                        "Can not set 'warnings_as_errors=true'; no logger is available. To solve, run: 'SET enable_logging=true;'",
                    ));
                }
                *self.warnings_as_errors.write().unwrap_or_else(|held| held.into_inner()) = false;
            }
            _ => unreachable!("the name was an honoured setting a moment ago"),
        }
        Ok(())
    }

    /// Takes a value for a setting rudb does not read, and refuses one that would be a promise.
    ///
    /// A `RESET` is a removal rather than a write of the default, so the map only ever holds the
    /// names a statement actually set and a default that changes does not leave stale copies behind.
    ///
    /// # Errors
    ///
    /// For a value the setting's own type cannot read, and for any value but the default of a
    /// setting whose default is the only thing rudb does.
    fn carry(&self, entry: &'static SettingEntry, value: Option<&Value>) -> Result<()> {
        let default = match entry.behaviour {
            Behaviour::Honoured => unreachable!("an honoured setting has a field of its own"),
            Behaviour::Knob(default) | Behaviour::DefaultOnly(default) => default,
        };
        let Some(value) = value else {
            self.carried.write().unwrap_or_else(|held| held.into_inner()).remove(entry.name);
            return Ok(());
        };
        let written = typed(entry, value)?;
        if matches!(entry.behaviour, Behaviour::DefaultOnly(_))
            && !written.eq_ignore_ascii_case(default)
        {
            return Err(Error::not_implemented(format!(
                "SET {} = '{written}' is not implemented. rudb behaves as if {} were '{default}' and takes no other value, because a setting it accepted and did not act on would be a wrong answer one statement later.",
                entry.name, entry.name
            )));
        }
        self.carried.write().unwrap_or_else(|held| held.into_inner()).insert(entry.name, written);
        Ok(())
    }

    /// Runs a pragma that is a statement, which is a `SET` with the name and the value in one word.
    ///
    /// Nine of the nineteen write nothing, because on the pin they move a flag `duckdb_settings()`
    /// does not list or they are deprecated and do nothing at all. Those succeed and leave
    /// everything where it was, which is what the pin does with them.
    ///
    /// # Errors
    ///
    /// For a name that is not one of the nineteen, in the words the catalog uses for a pragma it
    /// does not have.
    pub(crate) fn toggle(&self, name: &str) -> Result<()> {
        let Some(pragma) = rudb_functions::pragma_named(name) else {
            return Err(Error::catalog(format!(
                "Pragma Function with name {name} does not exist!"
            )));
        };
        let Some((setting, value)) = pragma.writes else {
            return Ok(());
        };
        let entry = rudb_functions::setting_named(setting)
            .expect("a pragma writes a setting the registry has, which its own test checks");
        self.carry(entry, Some(&Value::Varchar(value.to_string())))
    }

    /// What a setting rudb does not read is at now, which is its default until a statement sets it.
    fn carried(&self, entry: &SettingEntry) -> String {
        let default = match entry.behaviour {
            Behaviour::Honoured => unreachable!("an honoured setting has a field of its own"),
            Behaviour::Knob(default) | Behaviour::DefaultOnly(default) => default,
        };
        self.carried
            .read()
            .unwrap_or_else(|held| held.into_inner())
            .get(entry.name)
            .cloned()
            .unwrap_or_else(|| default.to_string())
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
        if is_links(name) {
            return Ok(self.links());
        }
        if let Some(which) = graph_size(name) {
            let sizes = self.sizes();
            return Ok(match which {
                Size::Cache => human(sizes.cache_bytes),
                Size::Narrow => sizes.narrow_bytes.to_string(),
            });
        }
        if is_rule(name) {
            return self.rules().named(name).map(|enabled| enabled.to_string()).ok_or_else(|| {
                Error::catalog(format!("no rule called {name}, the rules are {}", rule_names()))
            });
        }
        let Some(entry) = rudb_functions::setting_named(canonical(name)) else {
            return Err(Error::catalog(rudb_functions::unknown_setting(name)));
        };
        if entry.behaviour != Behaviour::Honoured {
            return Ok(self.carried(entry));
        }
        let config = self.config();
        match canonical(name) {
            "TimeZone" => {
                Ok(self.time_zone.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "allow_parser_override_extension" => Ok(self
                .allow_parser_override_extension
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .clone()),
            "current_dialect" => {
                Ok(self.current_dialect.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "dialect_compatibility_mode" => Ok(self
                .dialect_compatibility_mode
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .clone()),
            "default_order" => {
                Ok(self.default_order.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "default_null_order" => {
                Ok(self.default_null_order.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "disabled_optimizers" => Ok(self.disabled_optimizers()),
            "disable_timestamptz_casts" => Ok(self
                .disable_timestamptz_casts
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            "errors_as_json" => {
                Ok(self.errors_as_json.read().unwrap_or_else(|held| held.into_inner()).to_string())
            }
            "ieee_floating_point_ops" => Ok(self
                .ieee_floating_point_ops
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            "integer_division" => Ok(self
                .integer_division
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            // An unlimited budget prints as the word rather than as a number, because rudb's
            // default is no limit where DuckDB's is a fraction of the machine, and printing the
            // largest number a limit could be would be describing a limit that is not there.
            "memory_limit" => Ok(config.memory_limit().map_or("unlimited".to_string(), human)),
            "null_on_division_by_zero" => Ok(self
                .null_on_division_by_zero
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            "order_by_non_integer_literal" => Ok(self
                .order_by_non_integer_literal
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            "preserve_identifier_case" => Ok(self
                .preserve_identifier_case
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .clone()),
            "regex_match_operator_semantics" => Ok(self
                .regex_match_operator_semantics
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .clone()),
            "scalar_subquery_error_on_multiple_rows" => Ok(self
                .scalar_subquery_error_on_multiple_rows
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            "show_behavior" => {
                Ok(self.show_behavior.read().unwrap_or_else(|held| held.into_inner()).clone())
            }
            "threads" => Ok(config.threads().to_string()),
            "warnings_as_errors" => Ok(self
                .warnings_as_errors
                .read()
                .unwrap_or_else(|held| held.into_inner())
                .to_string()),
            _ => Err(Error::catalog(rudb_functions::unknown_setting(name))),
        }
    }

    /// Every setting and its value, for the table that lists them and the function that reads one.
    ///
    /// Built once per statement rather than held, because there are twenty two names and the alternative
    /// is a second copy of the settings that has to be kept in step with this one. An alias reports
    /// the same value as the name it resolves to, which is the same thing reading either spelling
    /// back gives, and it is what the binary returns for both halves of each pair.
    ///
    /// The two locks are taken once each here rather than once per name through [`Settings::value`],
    /// because every statement pays for this now that `current_setting()` can appear in any of them.
    /// Twenty settings and twenty two names means the loop below would otherwise take several locks.
    pub(crate) fn session(&self) -> Session {
        let config = self.config();
        let disabled = self.disabled_optimizers();
        let memory = config.memory_limit().map_or_else(|| "unlimited".to_string(), human);
        let threads = config.threads().to_string();
        let time_zone = self.time_zone.read().unwrap_or_else(|held| held.into_inner()).clone();
        let allow_parser_override_extension = self
            .allow_parser_override_extension
            .read()
            .unwrap_or_else(|held| held.into_inner())
            .clone();
        let default_order =
            self.default_order.read().unwrap_or_else(|held| held.into_inner()).clone();
        let current_dialect =
            self.current_dialect.read().unwrap_or_else(|held| held.into_inner()).clone();
        let dialect_compatibility_mode =
            self.dialect_compatibility_mode.read().unwrap_or_else(|held| held.into_inner()).clone();
        let default_null_order =
            self.default_null_order.read().unwrap_or_else(|held| held.into_inner()).clone();
        let disable_timestamptz_casts =
            *self.disable_timestamptz_casts.read().unwrap_or_else(|held| held.into_inner());
        let errors_as_json = *self.errors_as_json.read().unwrap_or_else(|held| held.into_inner());
        let ieee_floating_point_ops =
            *self.ieee_floating_point_ops.read().unwrap_or_else(|held| held.into_inner());
        let integer_division =
            *self.integer_division.read().unwrap_or_else(|held| held.into_inner());
        let null_on_division_by_zero =
            *self.null_on_division_by_zero.read().unwrap_or_else(|held| held.into_inner());
        let order_by_non_integer_literal =
            *self.order_by_non_integer_literal.read().unwrap_or_else(|held| held.into_inner());
        let preserve_identifier_case =
            self.preserve_identifier_case.read().unwrap_or_else(|held| held.into_inner()).clone();
        let regex_match_operator_semantics = self
            .regex_match_operator_semantics
            .read()
            .unwrap_or_else(|held| held.into_inner())
            .clone();
        let scalar_subquery_error_on_multiple_rows = *self
            .scalar_subquery_error_on_multiple_rows
            .read()
            .unwrap_or_else(|held| held.into_inner());
        let show_behavior =
            self.show_behavior.read().unwrap_or_else(|held| held.into_inner()).clone();
        let warnings_as_errors =
            *self.warnings_as_errors.read().unwrap_or_else(|held| held.into_inner());
        let mut session = Session::new();
        session.set_time_zone(&time_zone);
        session.set_default_descending(default_order == "DESC");
        session.set_default_null_order(match default_null_order.as_str() {
            "NULLS_FIRST" => DefaultNullOrder::First,
            "SQLITE" | "MYSQL" => DefaultNullOrder::Sqlite,
            "POSTGRES" => DefaultNullOrder::Postgres,
            _ => DefaultNullOrder::Last,
        });
        session.set_disable_timestamptz_casts(disable_timestamptz_casts);
        session.set_errors_as_json(errors_as_json);
        session.set_ieee_floating_point_ops(ieee_floating_point_ops);
        session.set_integer_division(integer_division);
        session.set_null_on_division_by_zero(null_on_division_by_zero);
        session.set_order_by_non_integer_literal(order_by_non_integer_literal);
        session.set_identifier_case(match preserve_identifier_case.as_str() {
            "lowercase" => IdentifierCase::Lower,
            "uppercase" => IdentifierCase::Upper,
            _ => IdentifierCase::Preserve,
        });
        session.set_regex_match_full(regex_match_operator_semantics.eq_ignore_ascii_case("full"));
        session.set_scalar_subquery_error_on_multiple_rows(scalar_subquery_error_on_multiple_rows);
        session.set_show_behavior(match show_behavior.to_ascii_uppercase().as_str() {
            "SETTING" => ShowBehavior::Setting,
            "TABLE" => ShowBehavior::Table,
            _ => ShowBehavior::Auto,
        });
        session.set_warnings_as_errors(warnings_as_errors);
        session.set_rules(self.rules());
        session.set_links(self.links());
        session.set_seams(self.seams().written());
        for entry in SETTINGS {
            if entry.behaviour != Behaviour::Honoured {
                session.set(entry.name, self.carried(entry));
                continue;
            }
            let name = entry.name;
            session.set(
                name,
                match canonical(name) {
                    "TimeZone" => time_zone.clone(),
                    "allow_parser_override_extension" => allow_parser_override_extension.clone(),
                    "current_dialect" => current_dialect.clone(),
                    "dialect_compatibility_mode" => dialect_compatibility_mode.clone(),
                    "default_order" => default_order.clone(),
                    "default_null_order" => default_null_order.clone(),
                    "disabled_optimizers" => disabled.clone(),
                    "disable_timestamptz_casts" => disable_timestamptz_casts.to_string(),
                    "errors_as_json" => errors_as_json.to_string(),
                    "ieee_floating_point_ops" => ieee_floating_point_ops.to_string(),
                    "integer_division" => integer_division.to_string(),
                    "null_on_division_by_zero" => null_on_division_by_zero.to_string(),
                    "memory_limit" => memory.clone(),
                    "order_by_non_integer_literal" => order_by_non_integer_literal.to_string(),
                    "preserve_identifier_case" => preserve_identifier_case.clone(),
                    "regex_match_operator_semantics" => regex_match_operator_semantics.clone(),
                    "scalar_subquery_error_on_multiple_rows" => {
                        scalar_subquery_error_on_multiple_rows.to_string()
                    }
                    "show_behavior" => show_behavior.clone(),
                    "threads" => threads.clone(),
                    "warnings_as_errors" => warnings_as_errors.to_string(),
                    other => unreachable!("{other} is not an honoured setting"),
                },
            );
        }
        session
    }

    fn replace(&self, config: Config) {
        *self.current.write().unwrap_or_else(|held| held.into_inner()) = config;
    }
}

/// The setting a name means, which is itself for every name but one half of each alias pair.
///
/// Seven settings have two spellings and the pin gives each spelling a row of its own, so one of the
/// two has to be the one everything else here reads and writes. For the three rudb acts on that is
/// the spelling the rest of the engine already uses, which is `memory_limit`, `threads` and
/// `default_null_order`. For the other four nothing here reads either spelling, so it is whichever
/// one the pin puts in the other's alias list, the way round the first two of the three landed
/// anyway.
///
/// A name that is not a setting comes back as written, so that whoever asked gets to say so.
const ALIASES: [(&str, &str); 7] = [
    ("max_memory", "memory_limit"),
    ("worker_threads", "threads"),
    ("null_order", "default_null_order"),
    ("checkpoint_threshold", "wal_autocheckpoint"),
    ("max_streaming_buffer_size", "streaming_buffer_size"),
    ("profiling_output", "profile_output"),
    ("username", "user"),
];

/// The setting a name means, in the spelling this file and the settings table both use for it.
fn canonical(name: &str) -> &str {
    for (written, meant) in ALIASES {
        if name.eq_ignore_ascii_case(written) {
            return meant;
        }
    }
    rudb_functions::setting_named(name).map_or(name, |entry| entry.name)
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
    if rudb_functions::setting_named(name).is_some() {
        return false;
    }
    name.starts_with(SEAM_PREFIX) || rudb_seam::seam_named(name).is_some()
}

/// Whether this name is the relationship declaration setting.
///
/// The same shape as [`is_seam`] and for the same reason: a DuckDB setting of this name, should one
/// ever exist, wins.
fn is_links(name: &str) -> bool {
    if rudb_functions::setting_named(name).is_some() {
        return false;
    }
    Session::is_links_setting(name)
}

/// Whether this name is the row order declaration setting.
///
/// The same shape as [`is_seam`] and for the same reason. DuckDB has no setting of either name
/// today, and if it ever takes one then the compatible answer wins and this loses its spelling.
pub(crate) fn is_clustering(name: &str) -> bool {
    if rudb_functions::setting_named(name).is_some() {
        return false;
    }
    rudb_common::is_clustering_setting(name)
}

/// Moves the catalog to exactly the declarations `written` names, and no others.
///
/// A write of the whole set rather than of one table, which is what makes it a setting rather than
/// a statement. `SET cluster_by = ''` and `RESET cluster_by` therefore take every declaration off
/// every table, which is what they look like they mean and is the only reading that leaves the
/// setting able to be read back as what it is.
///
/// Everything is resolved before anything is written. A setting naming three tables where the third
/// column name is a typo has to leave the catalog as it found it, because a partly applied
/// declaration is a layout nobody asked for and nothing left behind would say so.
fn declare(catalog: &mut Catalog, written: &str) -> Result<()> {
    let asked = parse_clustering(written)?;
    let mut wanted = Vec::with_capacity(asked.len());
    for declared in &asked {
        wanted.push(resolved(catalog, declared)?);
    }
    let cleared = catalog
        .tables()
        .filter(|table| table.clustering().is_some())
        .map(|table| table.name().clone())
        .collect::<Vec<_>>();
    for name in cleared {
        catalog.table_mut(&name)?.cluster_by(None)?;
    }
    for (name, clustering) in wanted {
        catalog.table_mut(&name)?.cluster_by(Some(clustering))?;
    }
    Ok(())
}

/// One written declaration as the table it names and the clustering over that table's columns.
fn resolved(catalog: &Catalog, declared: &Declared) -> Result<(QualifiedName, Clustering)> {
    let name = catalog.resolve(&parts(declared.table()))?;
    let fields = catalog.table(&name)?.columns();
    let mut columns = Vec::with_capacity(declared.columns().len());
    for column in declared.columns() {
        let at = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column));
        let at = at.ok_or_else(|| {
            Error::catalog(format!(
                "Table \"{}\" does not have a column named \"{column}\"",
                declared.table()
            ))
        })?;
        columns.push(u32::try_from(at).map_err(|_| Error::internal("a column past four billion"))?);
    }
    // A width the text named is taken as written and checked against the column's type, and a text
    // that named none leaves the width to the leading column, which is the whole of the difference
    // between the two constructors.
    let clustering = match declared.width() {
        Some(width) => Clustering::new(columns, width, fields)?,
        None => Clustering::over(columns, fields)?,
    };
    Ok((name, clustering))
}

/// A written table name cut at its dots, so a schema in front of it reaches the right table.
fn parts(table: &str) -> Vec<&str> {
    table.split('.').map(str::trim).collect()
}

/// Which of the two link join numbers a name is, if it is either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Size {
    /// How much of the parent has to fit for its hash table to stay cache resident.
    Cache,
    /// How wide the parent's projection may be before a hash join is worth its build.
    Narrow,
}

/// Whether this name is one of the two link join numbers.
///
/// The same shape as [`is_links`] and for the same reason. Both spellings of each, with a dot and
/// with an underscore, because that is what the seams and the relationship declaration both take.
fn graph_size(name: &str) -> Option<Size> {
    if rudb_functions::setting_named(name).is_some() {
        return None;
    }
    let named = |dotted: &str, under: &str| {
        name.eq_ignore_ascii_case(dotted) || name.eq_ignore_ascii_case(under)
    };
    if named("graph.cache_bytes", "graph_cache_bytes") {
        return Some(Size::Cache);
    }
    if named("graph.narrow_bytes", "graph_narrow_bytes") {
        return Some(Size::Narrow);
    }
    None
}

/// A byte count a value names, in either of the two ways somebody writes one.
///
/// `8MB` goes through the same parser `memory_limit` uses, so a unit means here what it means
/// there. A plain number is a count of bytes, which is the spelling a test writes and the one a
/// number with no unit can only mean.
///
/// # Errors
///
/// For text that is neither, and for a negative number.
fn size_of(value: &Value, name: &str) -> Result<u64> {
    if let Value::Varchar(text) = value {
        if text.contains(|character: char| character.is_ascii_alphabetic()) {
            return parse_size(text.trim());
        }
    }
    let count = integer_of(value, "UBIGINT")?;
    u64::try_from(count)
        .map_err(|_| Error::invalid_input(format!("{name} cannot be {count}, it is a byte count")))
}

/// Whether this name is one of the rule switches rather than a setting DuckDB has.
///
/// The same shape as [`is_seam`] and for the same reason. A DuckDB setting wins, so the day one of
/// its names collides with a rule of ours the compatible answer is the one that is given.
fn is_rule(name: &str) -> bool {
    if rudb_functions::setting_named(name).is_some() {
        return false;
    }
    looks_like_rule(name)
}

/// A rule switch as the boolean it sets.
///
/// `off` and `on` on top of what [`boolean_of`] takes, because that is how the specification
/// documents write these two switches and somebody reading them should be able to type what they
/// say. A rule setting is the only place those two words mean anything, so they are handled here
/// rather than in the cast every other boolean setting goes through.
fn switch_of(value: &Value) -> Result<bool> {
    match value {
        Value::Varchar(text) if text.eq_ignore_ascii_case("off") => Ok(false),
        Value::Varchar(text) if text.eq_ignore_ascii_case("on") => Ok(true),
        other => boolean_of(other),
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

/// A value in the text its setting's own type reads it back as.
///
/// The pin type checks a `SET` whether or not it goes on to read the setting, and rudb has to as
/// well, or `SET partitioned_write_max_open_files = 'blue'` is a number setting holding a word and
/// the mistake surfaces at whatever reads it instead of at the statement that made it. The types
/// here are the pin's spellings from the settings table, so this is a match on those and not on a
/// [`rudb_common::LogicalType`]. Anything else is text, which is what `VARCHAR`, `VARCHAR[]` and the
/// one `MAP` among them all want.
fn typed(entry: &SettingEntry, value: &Value) -> Result<String> {
    match entry.input_type {
        "BOOLEAN" => Ok(boolean_of(value)?.to_string()),
        "BIGINT" => Ok(integer_of(value, "INT64")?.to_string()),
        "UBIGINT" => {
            let count = integer_of(value, "UINT64")?;
            if count < 0 {
                return Err(Error::invalid_input(format!(
                    "Failed to cast value: Could not convert string '{count}' to UINT64"
                )));
            }
            Ok(count.to_string())
        }
        "DOUBLE" => Ok(double_of(value)?.to_string()),
        _ => Ok(text_of(value)),
    }
}

/// The whole number a value names, with the pin's type name in the sentence when it is not one.
fn integer_of(value: &Value, wanted: &str) -> Result<i128> {
    Ok(match value {
        Value::TinyInt(count) => i128::from(*count),
        Value::SmallInt(count) => i128::from(*count),
        Value::Integer(count) => i128::from(*count),
        Value::BigInt(count) => i128::from(*count),
        Value::HugeInt(count) => *count,
        Value::UTinyInt(count) => i128::from(*count),
        Value::USmallInt(count) => i128::from(*count),
        Value::UInteger(count) => i128::from(*count),
        Value::UBigInt(count) => i128::from(*count),
        Value::Varchar(text) => text.trim().parse::<i128>().map_err(|_| {
            Error::invalid_input(format!(
                "Failed to cast value: Could not convert string '{text}' to {wanted}"
            ))
        })?,
        other => {
            return Err(Error::invalid_input(format!(
                "Failed to cast value: Could not convert {} to {wanted}",
                other.logical_type()
            )));
        }
    })
}

/// The number a value names, for the one setting in the table whose type is `DOUBLE`.
fn double_of(value: &Value) -> Result<f64> {
    match value {
        Value::Float(number) => Ok(f64::from(*number)),
        Value::Double(number) => Ok(*number),
        Value::Varchar(text) => text.trim().parse::<f64>().map_err(|_| {
            Error::invalid_input(format!(
                "Failed to cast value: Could not convert string '{text}' to DOUBLE"
            ))
        }),
        other => integer_of(other, "DOUBLE").map(|count| count as f64),
    }
}

/// The thread count a value names.
fn threads_of(value: &Value) -> Result<usize> {
    let count = integer_of(value, "INT64")?;
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
    use super::{Catalog, Settings, bytes_of};
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
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "memory_limit",
                Scope::Unwritten,
                Some(&value),
            )
            .expect("a size");
        assert_eq!(memory.limit(), Some(1 << 30));
        assert_eq!(settings.value("memory_limit").expect("a setting"), "1.0 GiB");
        settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "memory_limit",
                Scope::Unwritten,
                None,
            )
            .expect("a reset");
        assert_eq!(memory.limit(), None, "reset goes back to what the database was opened with");
    }

    #[test]
    fn a_pass_that_is_not_a_pass_is_refused_by_the_statement_that_named_it() {
        let (settings, memory) = settings();
        let value = Value::Varchar("bogus".into());
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "disabled_optimizers",
                Scope::Unwritten,
                Some(&value),
            )
            .expect_err("not a pass");
        assert_eq!(error.code().duckdb_name(), "Parser Error");
        assert_eq!(settings.disabled_optimizers(), "", "a refused set changed nothing");
    }

    #[test]
    fn a_name_that_is_not_a_setting_says_so_and_offers_the_nearest_ones() {
        let (settings, memory) = settings();
        let error = settings
            .apply(&memory, &Pool::default(), &mut Catalog::new(), "bogus", Scope::Unwritten, None)
            .expect_err("not a setting");
        assert_eq!(error.code().duckdb_name(), "Catalog Error");
        assert_eq!(error.message(), "unrecognized configuration parameter \"bogus\"");
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "memory_limitt",
                Scope::Unwritten,
                None,
            )
            .expect_err("not a setting");
        assert!(error.message().contains("\"memory_limit\""), "{}", error.message());
    }

    #[test]
    fn the_two_scopes_this_database_does_not_have_are_two_different_sentences() {
        let (settings, memory) = settings();
        let value = Value::BigInt(2);
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "threads",
                Scope::Local,
                Some(&value),
            )
            .expect_err("no local scope");
        assert_eq!(error.message(), "SET LOCAL is not implemented.");
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "threads",
                Scope::Session,
                Some(&value),
            )
            .expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be set locally");
        // The word changes with the statement, because a writer who wrote `RESET` should not read a
        // sentence about `SET`.
        let error = settings
            .apply(&memory, &Pool::default(), &mut Catalog::new(), "threads", Scope::Local, None)
            .expect_err("no local scope");
        assert_eq!(error.message(), "RESET LOCAL is not implemented.");
        let error = settings
            .apply(&memory, &Pool::default(), &mut Catalog::new(), "threads", Scope::Session, None)
            .expect_err("no session copy");
        assert_eq!(error.message(), "option \"threads\" cannot be reset locally");
    }

    /// A knob is taken, kept and handed back, and nothing under it runs differently for it. The
    /// point of taking it at all is that a script that sets one in its preamble gets to keep going.
    #[test]
    fn a_setting_the_engine_does_not_read_is_taken_and_read_back() {
        let (settings, memory) = settings();
        let pool = Pool::default();
        assert_eq!(settings.value("enable_http_metadata_cache").expect("a setting"), "false");
        settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "enable_http_metadata_cache",
                Scope::Global,
                Some(&Value::Boolean(true)),
            )
            .expect("a knob takes a value");
        assert_eq!(settings.value("enable_http_metadata_cache").expect("a setting"), "true");
        // A `RESET` puts back the default, which here is forgetting rather than writing.
        settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "enable_http_metadata_cache",
                Scope::Global,
                None,
            )
            .expect("a knob resets");
        assert_eq!(settings.value("enable_http_metadata_cache").expect("a setting"), "false");
        // The type is checked even though nothing reads the value, so the mistake lands on the
        // statement that made it.
        let text = Value::Varchar("blue".to_string());
        let error = settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "enable_http_metadata_cache",
                Scope::Global,
                Some(&text),
            )
            .expect_err("not a boolean");
        assert_eq!(
            error.message(),
            "Failed to cast value: Could not convert string 'blue' to BOOL"
        );
        let error = settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "partitioned_write_max_open_files",
                Scope::Global,
                Some(&text),
            )
            .expect_err("not a number");
        assert_eq!(
            error.message(),
            "Failed to cast value: Could not convert string 'blue' to UINT64"
        );
    }

    /// The other half of the rule. A setting that would change an answer is taken at the value rudb
    /// already behaves as and refused at every other, because taking it and doing nothing about it
    /// would turn one clear error into a wrong answer a statement later.
    #[test]
    fn a_setting_that_would_change_an_answer_is_taken_only_at_its_default() {
        let (settings, memory) = settings();
        let pool = Pool::default();
        settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "preserve_insertion_order",
                Scope::Global,
                Some(&Value::Boolean(true)),
            )
            .expect("the value it already behaves as");
        let error = settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "preserve_insertion_order",
                Scope::Global,
                Some(&Value::Boolean(false)),
            )
            .expect_err("rudb cannot stop preserving it");
        assert_eq!(error.code().duckdb_name(), "Not implemented Error");
        assert!(error.message().contains("rudb behaves as if"), "{error}");
        // A reset is always fine, since it is asking for what it already is.
        settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "preserve_insertion_order",
                Scope::Global,
                None,
            )
            .expect("a reset asks for the default");
    }

    /// Fifteen of the carried names are per connection on the pin. rudb has one connection's worth
    /// of state and reads none of the fifteen, so all three spellings are taken and land together.
    #[test]
    fn a_setting_the_pin_keeps_per_connection_takes_all_three_scopes() {
        let (settings, memory) = settings();
        let pool = Pool::default();
        let renderer = Value::Varchar("json".to_string());
        for scope in [Scope::Global, Scope::Session, Scope::Local, Scope::Unwritten] {
            settings
                .apply(
                    &memory,
                    &pool,
                    &mut Catalog::new(),
                    "profiling_renderer_settings",
                    scope,
                    Some(&renderer),
                )
                .expect("a local setting takes every scope");
        }
        assert_eq!(settings.value("profiling_renderer_settings").expect("a setting"), "json");
    }

    /// Seven settings have two spellings, and both spellings are one setting however they are mixed.
    #[test]
    fn either_spelling_of_an_alias_pair_writes_what_the_other_one_reads() {
        let (settings, memory) = settings();
        let pool = Pool::default();
        let pairs = [
            ("checkpoint_threshold", "wal_autocheckpoint", "32.0 MiB"),
            ("max_streaming_buffer_size", "streaming_buffer_size", "1.0 MiB"),
            ("profiling_output", "profile_output", "somewhere"),
            ("username", "user", "someone"),
        ];
        for (written, other, value) in pairs {
            let held = Value::Varchar(value.to_string());
            settings
                .apply(&memory, &pool, &mut Catalog::new(), written, Scope::Global, Some(&held))
                .expect("a setting");
            assert_eq!(settings.value(other).expect("a setting"), value, "{written}");
            settings
                .apply(&memory, &pool, &mut Catalog::new(), other, Scope::Global, None)
                .expect("a setting");
            assert_eq!(
                settings.value(written).expect("a setting"),
                settings.value(other).expect("a setting"),
                "{written}"
            );
        }
    }

    #[test]
    fn a_thread_count_is_a_whole_number_of_at_least_one() {
        let (settings, memory) = settings();
        let pool = Pool::new(1);
        settings
            .apply(
                &memory,
                &pool,
                &mut Catalog::new(),
                "threads",
                Scope::Global,
                Some(&Value::BigInt(4)),
            )
            .expect("four threads");
        assert_eq!(settings.value("threads").expect("a setting"), "4");
        assert_eq!(pool.threads(), 4, "the setting reached the thing that hands out threads");
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "threads",
                Scope::Global,
                Some(&Value::BigInt(0)),
            )
            .expect_err("no threads at all");
        assert_eq!(error.message(), "Must have at least 1 thread!");
        let text = Value::Varchar("abc".into());
        let error = settings
            .apply(
                &memory,
                &Pool::default(),
                &mut Catalog::new(),
                "threads",
                Scope::Global,
                Some(&text),
            )
            .expect_err("not a number");
        assert_eq!(
            error.message(),
            "Failed to cast value: Could not convert string 'abc' to INT64"
        );
    }
}
