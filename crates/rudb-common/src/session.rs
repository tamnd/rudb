//! What a session has set, for the tables and functions that read a setting back.
//!
//! Here for the layer rule and not because a setting is a kind of value, which is the same reason
//! [`crate::Cancel`] is here. The thing that fills this in is the embedding API at rank 13, which is
//! the only place that knows what `SET memory_limit` left behind, and the thing that reads it is the
//! executor at rank 12, where `duckdb_settings()` is built. No two crates in between can see each
//! other, so the only place both of them can see is the bottom.
//!
//! Strings on both sides, rather than a value per setting. A setting is written as text by `SET`,
//! read back as text by `current_setting()` and printed as text by `duckdb_settings()`, and the one
//! place the type matters is the `input_type` column, which is a fact about the setting rather than
//! about the session. Holding a `Value` here would mean the rendering happened twice, once for each
//! reader, and the two would eventually disagree about how many decimal places a memory limit has.

use std::collections::BTreeMap;

use chrono::{Offset, TimeZone as _, Utc};
use chrono_tz::Tz;

/// The settings a session has, by name.
///
/// Every setting the engine has, not only the ones somebody changed. A reader of this is answering
/// "what is it now", so a name that is missing means the engine does not have that setting rather
/// than that it is at its default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    values: BTreeMap<String, String>,
    time_zone: Tz,
    semantics: Semantics,
}

/// The meaning-changing session choices consumed while a query is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Semantics {
    default_descending: bool,
    default_null_order: DefaultNullOrder,
    disable_timestamptz_casts: bool,
    integer_division: bool,
    ieee_floating_point_ops: bool,
    identifier_case: IdentifierCase,
    null_on_division_by_zero: bool,
    order_by_non_integer_literal: bool,
    regex_match_full: bool,
    show_behavior: ShowBehavior,
    warnings_as_errors: bool,
}

impl Default for Semantics {
    fn default() -> Self {
        Self {
            default_descending: false,
            default_null_order: DefaultNullOrder::default(),
            disable_timestamptz_casts: false,
            integer_division: false,
            ieee_floating_point_ops: true,
            identifier_case: IdentifierCase::Preserve,
            null_on_division_by_zero: false,
            order_by_non_integer_literal: false,
            regex_match_full: false,
            show_behavior: ShowBehavior::Auto,
            warnings_as_errors: false,
        }
    }
}

impl Semantics {
    /// How unquoted identifiers are folded while a statement is parsed.
    #[must_use]
    pub fn identifier_case(self) -> IdentifierCase {
        self.identifier_case
    }
    /// Whether casts from local timestamps to zoned timestamps are refused.
    #[must_use]
    pub fn disable_timestamptz_casts(self) -> bool {
        self.disable_timestamptz_casts
    }

    /// Whether floating division and remainder use IEEE answers for zero divisors.
    #[must_use]
    pub fn ieee_floating_point_ops(self) -> bool {
        self.ieee_floating_point_ops
    }

    /// Whether an order item with no direction is descending.
    #[must_use]
    pub fn default_descending(self) -> bool {
        self.default_descending
    }

    /// Whether nulls precede values for an unstated placement in this direction.
    #[must_use]
    pub fn nulls_first(self, descending: bool) -> bool {
        match self.default_null_order {
            DefaultNullOrder::First => true,
            DefaultNullOrder::Last => false,
            DefaultNullOrder::Sqlite => !descending,
            DefaultNullOrder::Postgres => descending,
        }
    }

    /// Whether `/` is bound as the integer division operator.
    #[must_use]
    pub fn integer_division(self) -> bool {
        self.integer_division
    }

    /// Whether a division that would raise on a zero divisor yields null instead.
    #[must_use]
    pub fn null_on_division_by_zero(self) -> bool {
        self.null_on_division_by_zero
    }

    /// Whether a constant non-integer expression is accepted as a sort key.
    #[must_use]
    pub fn order_by_non_integer_literal(self) -> bool {
        self.order_by_non_integer_literal
    }

    /// Whether regex match operators require the entire string to match.
    #[must_use]
    pub fn regex_match_full(self) -> bool {
        self.regex_match_full
    }

    /// How a bare name following `SHOW` is resolved.
    #[must_use]
    pub fn show_behavior(self) -> ShowBehavior {
        self.show_behavior
    }

    /// Whether warnings are promoted to errors.
    #[must_use]
    pub fn warnings_as_errors(self) -> bool {
        self.warnings_as_errors
    }
}

/// How a session folds identifiers that were not quoted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdentifierCase {
    /// Keep the spelling in the statement.
    #[default]
    Preserve,
    /// Fold ASCII letters to lowercase.
    Lower,
    /// Fold ASCII letters to uppercase.
    Upper,
}

/// How `SHOW name` chooses between a setting and a table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShowBehavior {
    /// Prefer a table when one exists, then fall back to a setting.
    #[default]
    Auto,
    /// Always read a setting.
    Setting,
    /// Always describe a table.
    Table,
}

/// How an unstated `NULLS FIRST` or `NULLS LAST` is resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DefaultNullOrder {
    /// Nulls precede values in both directions.
    First,
    /// Nulls follow values in both directions, which is DuckDB's default.
    #[default]
    Last,
    /// Nulls are low, as in SQLite and MySQL.
    Sqlite,
    /// Nulls are high, as in PostgreSQL.
    Postgres,
}

/// A parsed session time zone, cheap enough to carry beside a prepared expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimeZone(Tz);

impl Default for SessionTimeZone {
    fn default() -> Self {
        Self(chrono_tz::UTC)
    }
}

impl SessionTimeZone {
    /// The UTC offset in seconds at an instant expressed as Unix microseconds.
    #[must_use]
    pub fn offset_seconds_at(self, micros: i64) -> i32 {
        let seconds = micros.div_euclid(1_000_000);
        let nanos = u32::try_from(micros.rem_euclid(1_000_000) * 1_000).unwrap_or_default();
        let Some(utc) = Utc.timestamp_opt(seconds, nanos).single() else { return 0 };
        self.0.offset_from_utc_datetime(&utc.naive_utc()).fix().local_minus_utc()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self { values: BTreeMap::new(), time_zone: chrono_tz::UTC, semantics: Semantics::default() }
    }
}

impl Session {
    /// A session that knows nothing, which is what a caller with no database behind it has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what one setting is now.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.values.insert(name.to_string(), value.into());
    }

    /// Sets the time zone after it has been validated by the setting layer.
    pub fn set_time_zone(&mut self, name: &str) {
        self.time_zone = name.parse().unwrap_or(chrono_tz::UTC);
    }

    /// The canonical IANA name of the session time zone.
    #[must_use]
    pub fn time_zone(&self) -> &str {
        self.time_zone.name()
    }

    /// The parsed zone used by prepared expressions.
    #[must_use]
    pub fn session_time_zone(&self) -> SessionTimeZone {
        SessionTimeZone(self.time_zone)
    }

    /// Sets the direction used by an order item that names none.
    pub fn set_default_descending(&mut self, descending: bool) {
        self.semantics.default_descending = descending;
    }

    /// Sets how an order item with no null placement is resolved.
    pub fn set_default_null_order(&mut self, order: DefaultNullOrder) {
        self.semantics.default_null_order = order;
    }

    /// Sets whether `/` is bound as the integer division operator.
    pub fn set_integer_division(&mut self, enabled: bool) {
        self.semantics.integer_division = enabled;
    }

    /// Sets whether floating division and remainder use IEEE answers for zero divisors.
    pub fn set_ieee_floating_point_ops(&mut self, enabled: bool) {
        self.semantics.ieee_floating_point_ops = enabled;
    }

    /// Sets how unquoted identifiers are folded while a statement is parsed.
    pub fn set_identifier_case(&mut self, case: IdentifierCase) {
        self.semantics.identifier_case = case;
    }

    /// Sets whether division errors caused by a zero divisor become nulls.
    pub fn set_null_on_division_by_zero(&mut self, enabled: bool) {
        self.semantics.null_on_division_by_zero = enabled;
    }

    /// Sets whether a constant non-integer expression is accepted as a sort key.
    pub fn set_order_by_non_integer_literal(&mut self, enabled: bool) {
        self.semantics.order_by_non_integer_literal = enabled;
    }

    /// Sets whether casts from local timestamps to zoned timestamps are refused.
    pub fn set_disable_timestamptz_casts(&mut self, enabled: bool) {
        self.semantics.disable_timestamptz_casts = enabled;
    }

    /// Sets whether regex match operators require the entire string to match.
    pub fn set_regex_match_full(&mut self, enabled: bool) {
        self.semantics.regex_match_full = enabled;
    }

    /// Sets how `SHOW name` resolves its name.
    pub fn set_show_behavior(&mut self, behavior: ShowBehavior) {
        self.semantics.show_behavior = behavior;
    }

    /// Sets whether warnings are promoted to errors.
    pub fn set_warnings_as_errors(&mut self, enabled: bool) {
        self.semantics.warnings_as_errors = enabled;
    }

    /// The meaning-changing choices the binder resolves into the plan.
    #[must_use]
    pub fn semantics(&self) -> Semantics {
        self.semantics
    }

    /// Whether the bundled time-zone database knows this name.
    #[must_use]
    pub fn knows_time_zone(name: &str) -> bool {
        name.parse::<Tz>().is_ok()
    }

    /// The UTC offset in seconds at an instant expressed as Unix microseconds.
    #[must_use]
    pub fn offset_seconds_at(&self, micros: i64) -> i32 {
        self.session_time_zone().offset_seconds_at(micros)
    }

    /// A UTC instant shifted to the wall clock of this session.
    #[must_use]
    pub fn local_micros(&self, micros: i64) -> i64 {
        micros.saturating_add(i64::from(self.offset_seconds_at(micros)) * 1_000_000)
    }

    /// What that setting is now, and `None` for a name this session has no answer for.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Every setting and its value, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn a_session_hands_back_what_was_put_in_and_says_nothing_about_a_name_it_has_not_got() {
        let mut session = Session::new();
        assert!(session.is_empty());
        session.set("threads", "8");
        session.set("memory_limit", "1.0 GiB");
        assert_eq!(session.get("threads"), Some("8"));
        assert_eq!(session.get("nothing_called_this"), None);
        // Name order, because the one reader of this is a catalog table that comes out sorted and
        // sorting it twice would be sorting it once too many.
        let pairs: Vec<(&str, &str)> = session.iter().collect();
        assert_eq!(pairs, [("memory_limit", "1.0 GiB"), ("threads", "8")]);
    }
}
