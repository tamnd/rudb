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
use std::sync::Arc;

use chrono::{LocalResult, Offset, TimeDelta, TimeZone as _, Utc};
use chrono_tz::Tz;

use crate::Rules;
use crate::types::LogicalType;
use crate::value::Value;

/// The settings a session has, by name.
///
/// Every setting the engine has, not only the ones somebody changed. A reader of this is answering
/// "what is it now", so a name that is missing means the engine does not have that setting rather
/// than that it is at its default.
///
/// The names and values are shared between copies. The database builds a session once and hands a
/// copy to every statement, and with a couple of hundred settings a deep copy of the map was most
/// of what a `SELECT 1` cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    values: Arc<BTreeMap<String, String>>,
    time_zone: Tz,
    semantics: Semantics,
    rules: Rules,
    links: String,
    seams: String,
    variables: Variables,
    postgres: Postgreses,
    begun: Begun,
    transaction: Transaction,
    prepared: Prepareds,
}

/// When the open transaction began and when the statement arrived, in microseconds since the
/// epoch, or none.
///
/// Every copy compares equal, so a plan cached against a session is not lost at each statement.
/// That is safe because a plan that reads the instant is never cached, see `simple_cacheable`.
#[derive(Debug, Clone, Copy, Default)]
struct Begun {
    transaction: Option<i64>,
    statement: Option<i64>,
}

impl PartialEq for Begun {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Begun {}

/// What a PostgreSQL session gives the engine to read: its parameters and the text of `version()`.
///
/// The server owns the parameters and changes them. The engine gets a new copy each time they
/// change, which is cheap because the parameters hold only what the session touched.
#[derive(Debug, Clone)]
pub struct Postgres {
    /// The parameters of the session.
    pub settings: crate::guc::Settings,
    /// What `version()` returns.
    pub version: String,
    /// The input functions of the types, which read the text of a string literal in a cast.
    pub input: Option<Arc<dyn LiteralInput>>,
    /// The backend number of the session, which holds its advisory locks in [`crate::advisory`].
    pub backend: i32,
    /// The OID of the database of the session, which is part of the key of an advisory lock.
    pub database: u32,
}

/// Reads the text of a literal such as `'infinity'::date` with the input function of the type, as
/// PostgreSQL does, and not with the cast of the engine.
pub trait LiteralInput: Send + Sync + std::fmt::Debug {
    /// The value of `text` as the PostgreSQL type `oid`, or `None` when the type has no input
    /// function here.
    fn read(&self, oid: u32, text: &str) -> Option<crate::Result<Value>>;
}

/// The PostgreSQL session, if there is one, compared by identity. A new copy is a new value, so a
/// cached plan that read the old copy is not used again.
#[derive(Debug, Clone, Default)]
struct Postgreses(Option<Arc<Postgres>>);

impl PartialEq for Postgreses {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(one), Some(other)) => Arc::ptr_eq(one, other),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for Postgreses {}

/// The number of the transaction a statement runs in, which `txid_current()` answers with.
///
/// Every copy compares equal, because the number changes with every statement outside a block and
/// a plan kept for a statement's text would otherwise never be used twice. No kept plan reads it:
/// `txid_current()` is a call, and a statement with a call in it is not kept.
#[derive(Debug, Clone, Copy, Default)]
struct Transaction(u64);

impl PartialEq for Transaction {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Transaction {}

/// One statement `PREPARE` named, the way `duckdb_prepared_statements()` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStatement {
    /// The name as it was written.
    pub name: String,
    /// The statement written back out, which is how the pin prints it rather than how it was typed.
    pub statement: String,
    /// How many parameters it takes. The pin lists each one as `UNKNOWN`, whatever it is used as.
    pub parameters: usize,
    /// The types of the columns it answers, `BIGINT` alone for a write with no `RETURNING`. `None`
    /// where the pin plans the statement again at every `EXECUTE` and so has no types to give.
    pub results: Option<Vec<LogicalType>>,
}

/// The statements a connection holds by name, in the order of their names in lower case.
///
/// Shared, so reading the session once per statement copies a pointer and not the list. Two are
/// equal when they hold the same statements, so the session a plan was kept against stops matching
/// once a statement is prepared or deallocated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Prepareds(Arc<[PreparedStatement]>);

/// One value `SET VARIABLE` left behind, with the type it was computed at.
///
/// The type is held beside the value rather than read off it, because a variable set to a query
/// that found no row is a null that is still an INTEGER, and `typeof(getvariable('a'))` says so on
/// the pin.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    /// The name as it was written, which is what `duckdb_variables()` lists.
    pub name: String,
    /// What the expression came to.
    pub value: Value,
    /// The type the expression had.
    pub ty: LogicalType,
}

/// The variables of a session, in the order they were first set.
///
/// Shared between copies for the reason the settings are, and compared by what they hold so that a
/// session is still something a cached plan can be checked against. A value is compared with its
/// own equality, so a variable holding a NaN makes two sessions differ, which costs a cached plan
/// and nothing else.
#[derive(Debug, Clone, Default)]
struct Variables(Arc<Vec<Variable>>);

impl PartialEq for Variables {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl Eq for Variables {}

/// The meaning-changing session choices consumed while a query is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Semantics {
    default_descending: bool,
    default_null_order: DefaultNullOrder,
    disable_timestamptz_casts: bool,
    errors_as_json: bool,
    integer_division: bool,
    ieee_floating_point_ops: bool,
    identifier_case: IdentifierCase,
    null_on_division_by_zero: bool,
    order_by_non_integer_literal: bool,
    regex_match_full: bool,
    scalar_subquery_error_on_multiple_rows: bool,
    show_behavior: ShowBehavior,
    single_arrow_lambdas: bool,
    warnings_as_errors: bool,
}

impl Default for Semantics {
    fn default() -> Self {
        Self {
            default_descending: false,
            default_null_order: DefaultNullOrder::default(),
            disable_timestamptz_casts: false,
            errors_as_json: false,
            integer_division: false,
            ieee_floating_point_ops: true,
            identifier_case: IdentifierCase::Preserve,
            null_on_division_by_zero: false,
            order_by_non_integer_literal: false,
            regex_match_full: false,
            scalar_subquery_error_on_multiple_rows: true,
            show_behavior: ShowBehavior::Auto,
            single_arrow_lambdas: false,
            warnings_as_errors: false,
        }
    }
}

impl Semantics {
    /// Whether errors are returned as structured JSON.
    #[must_use]
    pub fn errors_as_json(self) -> bool {
        self.errors_as_json
    }
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

    /// Whether a lambda may be written with the deprecated arrow, `x -> x + 1`, which is what
    /// `lambda_syntax = 'ENABLE_SINGLE_ARROW'` turns on.
    #[must_use]
    pub fn single_arrow_lambdas(self) -> bool {
        self.single_arrow_lambdas
    }

    /// Whether a scalar query producing several rows raises an error.
    #[must_use]
    pub fn scalar_subquery_error_on_multiple_rows(self) -> bool {
        self.scalar_subquery_error_on_multiple_rows
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

/// Seconds in 400 Gregorian years, after which the calendar and every rule written in it repeat.
const CYCLE_SECONDS: i64 = 146_097 * 86_400;

/// Seconds either side of 1970 that chrono can turn into a date, which stops near the year 262143.
const HELD_SECONDS: i64 = 8_000_000_000_000;

/// The same moment of the 400 year cycle moved inside what chrono holds.
///
/// A timestamp reaches the year 294247 and chrono does not, but a zone's offset only depends on
/// where in the cycle a moment is, so the offset of the moved moment is the offset of the real one.
fn held(seconds: i64) -> i64 {
    if seconds.abs() <= HELD_SECONDS {
        return seconds;
    }
    let cycles = (seconds.abs() - HELD_SECONDS) / CYCLE_SECONDS + 1;
    seconds - seconds.signum() * cycles * CYCLE_SECONDS
}

/// The first second of 2038, where the bundled zone tables stop listing transitions.
const TABLE_END: i64 = 2_145_916_800;

/// The years a moment after [`TABLE_END`] is moved into, which are recent enough to follow the
/// rules a zone has now and long enough to hold every layout a year can have.
const RULE_YEARS: std::ops::RangeInclusive<i32> = 2010..=2037;

fn leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

impl SessionTimeZone {
    /// The offset chrono holds for a moment it can turn into a date.
    fn listed_offset(self, seconds: i64) -> i32 {
        let Some(utc) = Utc.timestamp_opt(seconds, 0).single() else { return 0 };
        self.0.offset_from_utc_datetime(&utc.naive_utc()).fix().local_minus_utc()
    }

    /// The same moment of a year that the zone tables list, which has the offset the real moment
    /// has under the zone's current rules.
    ///
    /// The tables stop in 2037, and after that chrono answers the last offset it listed forever,
    /// which is winter time in New York for the rest of the calendar. ICU carries the zone's last
    /// rule on instead, so a zone still changing its clocks in 2037 gets the moment moved to one of
    /// [`RULE_YEARS`] that starts on the same weekday and is a leap year exactly when the real year
    /// is, where every rule written as a weekday of a month lands on the same dates.
    fn listed(self, seconds: i64) -> i64 {
        let seconds = held(seconds);
        if seconds < TABLE_END
            || self.listed_offset(TABLE_END - 183 * 86_400) == self.listed_offset(TABLE_END - 1)
        {
            return seconds;
        }
        let Ok(days) = i32::try_from(seconds.div_euclid(86_400)) else { return seconds };
        let (year, _, _) = crate::civil_from_days(days);
        let start = i64::from(crate::days_from_civil(year, 1, 1));
        RULE_YEARS
            .rev()
            .map(|candidate| (candidate, i64::from(crate::days_from_civil(candidate, 1, 1))))
            .find(|(candidate, first)| {
                leap(*candidate) == leap(year) && (start - first).rem_euclid(7) == 0
            })
            .map_or(seconds, |(_, first)| seconds - (start - first) * 86_400)
    }

    /// The zone a name spells, with case ignored the way the pin's ICU lookup ignores it, or `None`
    /// for a name the bundled time zone database does not know.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        if let Ok(zone) = name.parse::<Tz>() {
            return Some(Self(zone));
        }
        chrono_tz::TZ_VARIANTS
            .iter()
            .find(|zone| zone.name().eq_ignore_ascii_case(name))
            .map(|zone| Self(*zone))
    }

    /// Whether the zone is UTC, where every wall clock is its instant and nothing needs moving.
    #[must_use]
    pub fn is_utc(self) -> bool {
        matches!(self.0, chrono_tz::UTC | chrono_tz::Etc::UTC)
    }

    /// The canonical IANA name of the zone.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// The UTC offset in seconds at an instant expressed as Unix microseconds.
    #[must_use]
    pub fn offset_seconds_at(self, micros: i64) -> i32 {
        self.listed_offset(self.listed(micros.div_euclid(1_000_000)))
    }

    /// The UTC offset in seconds right now, which is the offset the pin puts on a time of day that
    /// came without one, since a time of day has no date to look an offset up at.
    #[must_use]
    pub fn offset_seconds_now(self) -> i32 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX));
        self.offset_seconds_at(now)
    }

    /// The wall clock this zone reads at an instant, both as Unix microseconds, or `None` when the
    /// reading is past the end of the `i64`.
    #[must_use]
    pub fn local_of_instant(self, micros: i64) -> Option<i64> {
        micros.checked_add(i64::from(self.offset_seconds_at(micros)) * 1_000_000)
    }

    /// The instant a wall clock reading in this zone names, both as Unix microseconds.
    ///
    /// This is what an ICU calendar answers when its fields are set, which settles the two readings
    /// that do not name exactly one instant the way ICU's defaults do. A reading the clocks skipped
    /// over is read with the offset from before the jump, so 02:30 on the morning New York moves to
    /// summer time is 03:30 summer time. A reading the clocks passed twice is the second of the two.
    /// `None` when the instant is past the end of the `i64`.
    #[must_use]
    pub fn instant_of_local(self, micros: i64) -> Option<i64> {
        let seconds = self.listed(micros.div_euclid(1_000_000));
        let local = Utc.timestamp_opt(seconds, 0).single()?.naive_utc();
        let offset = match self.0.offset_from_local_datetime(&local) {
            LocalResult::Single(offset) | LocalResult::Ambiguous(_, offset) => offset.fix(),
            LocalResult::None => {
                self.0.offset_from_utc_datetime(&(local - TimeDelta::days(1))).fix()
            }
        };
        micros.checked_sub(i64::from(offset.local_minus_utc()) * 1_000_000)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            values: Arc::new(BTreeMap::new()),
            time_zone: chrono_tz::UTC,
            semantics: Semantics::default(),
            rules: Rules::new(),
            links: String::new(),
            seams: String::new(),
            variables: Variables::default(),
            postgres: Postgreses::default(),
            begun: Begun::default(),
            transaction: Transaction::default(),
            prepared: Prepareds::default(),
        }
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
        Arc::make_mut(&mut self.values).insert(name.to_string(), value.into());
    }

    /// Sets the time zone after it has been validated by the setting layer.
    pub fn set_time_zone(&mut self, name: &str) {
        self.time_zone = SessionTimeZone::named(name).map_or(chrono_tz::UTC, |zone| zone.0);
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

    /// Sets whether errors are returned as structured JSON.
    pub fn set_errors_as_json(&mut self, enabled: bool) {
        self.semantics.errors_as_json = enabled;
    }

    /// Sets whether regex match operators require the entire string to match.
    pub fn set_regex_match_full(&mut self, enabled: bool) {
        self.semantics.regex_match_full = enabled;
    }

    /// Sets whether a lambda may be written with the deprecated arrow.
    pub fn set_single_arrow_lambdas(&mut self, enabled: bool) {
        self.semantics.single_arrow_lambdas = enabled;
    }

    /// Sets whether scalar queries may choose one row from several.
    pub fn set_scalar_subquery_error_on_multiple_rows(&mut self, enabled: bool) {
        self.semantics.scalar_subquery_error_on_multiple_rows = enabled;
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

    /// Records which optimization rules the session has turned off.
    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    /// Which optimization rules may fire for this statement.
    ///
    /// Read by whatever is about to apply one, which is why it rides on the session rather than
    /// being reached for through the database: the rank that sets it and the ranks that obey it
    /// cannot see each other.
    #[must_use]
    pub fn rules(&self) -> Rules {
        self.rules
    }

    /// Records the relationships `SET graph_links` declared, as they were written.
    pub fn set_links(&mut self, links: impl Into<String>) {
        self.links = links.into();
    }

    /// The relationships declared for this session, as they were written, empty for none.
    ///
    /// The text and not the parsed form, because the parser is in `rudb-graph` and a session is
    /// read by ranks below that one. Whoever needs a relationship is above it and parses this
    /// itself, and the text is what `SET` already validated, so the parse there cannot fail.
    #[must_use]
    pub fn links(&self) -> &str {
        &self.links
    }

    /// Records which seams the session has pinned, as a hint body.
    ///
    /// The text and not the parsed form, for the reason [`Session::links`] carries text: the pins
    /// live in `rudb-seam`, which is above this crate, and a session is read from below it. A hint
    /// body rather than a format of its own because `rudb_seam::Settings` already writes one and
    /// already parses one, so the spelling of a pin cannot come to mean two things.
    pub fn set_seams(&mut self, seams: impl Into<String>) {
        self.seams = seams.into();
    }

    /// The seams this session has pinned, as a hint body, empty for none.
    ///
    /// Empty is not the same as unknown. A session with nothing pinned has every seam at its
    /// default, which is what an unpinned seam reads back as, so a reader of this parses it and asks
    /// it rather than treating empty as an absence of an answer.
    #[must_use]
    pub fn seams(&self) -> &str {
        &self.seams
    }

    /// Records the variables `SET VARIABLE` has left, in the order they were first set.
    pub fn set_variables(&mut self, variables: Vec<Variable>) {
        self.variables = Variables(Arc::new(variables));
    }

    /// The variable of that name, which is matched without regard to case the way the pin matches
    /// it, so `SET VARIABLE A = 1` is read back by `getvariable('a')`.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.0.iter().find(|held| held.name.eq_ignore_ascii_case(name))
    }

    /// Every variable, in the order they were first set.
    pub fn variables(&self) -> impl Iterator<Item = &Variable> {
        self.variables.0.iter()
    }

    /// Records the PostgreSQL session that runs the statements, or none.
    ///
    /// A PostgreSQL session also takes the rules of PostgreSQL for division: `/` of two integers
    /// is an integer, and a zero divisor is an error for every type.
    pub fn set_postgres(&mut self, postgres: Option<Arc<Postgres>>) {
        if postgres.is_some() {
            self.semantics.integer_division = true;
            self.semantics.ieee_floating_point_ops = false;
            self.semantics.null_on_division_by_zero = false;
        }
        self.postgres = Postgreses(postgres);
    }

    /// Records when the open transaction began, in microseconds since the epoch, or none.
    pub fn set_begun(&mut self, begun: Option<i64>) {
        self.begun.transaction = begun;
    }

    /// Records when the client sent the statement, in microseconds since the epoch, or none.
    pub fn set_statement_start(&mut self, start: Option<i64>) {
        self.begun.statement = start;
    }

    /// When the client sent the statement, which `statement_timestamp()` gives. A server records
    /// it when it reads the message, so all the statements of one simple query have the same one,
    /// as in PostgreSQL.
    #[must_use]
    pub fn statement_start(&self) -> Option<i64> {
        self.begun.statement
    }

    /// When the open transaction began. `now()` and `current_timestamp` give this instant in each
    /// statement of the transaction, as in PostgreSQL and DuckDB. Outside a transaction each
    /// statement is one, and they give the instant the statement started.
    #[must_use]
    pub fn begun(&self) -> Option<i64> {
        self.begun.transaction
    }

    /// The PostgreSQL session that runs the statements. `current_setting()`, `version()` and the
    /// user functions read it when it is there.
    #[must_use]
    pub fn postgres(&self) -> Option<&Postgres> {
        self.postgres.0.as_deref()
    }

    /// Records the number of the transaction the statement runs in.
    pub fn set_transaction(&mut self, number: u64) {
        self.transaction = Transaction(number);
    }

    /// Records the statements the connection holds by name.
    pub fn set_prepared(&mut self, statements: Arc<[PreparedStatement]>) {
        self.prepared = Prepareds(statements);
    }

    /// The statements the connection holds by name, which `duckdb_prepared_statements()` lists.
    #[must_use]
    pub fn prepared(&self) -> &[PreparedStatement] {
        &self.prepared.0
    }

    /// The number of the transaction the statement runs in, which is the same for every statement
    /// of a block and different for every transaction, and zero for a session no database made.
    #[must_use]
    pub fn transaction(&self) -> u64 {
        self.transaction.0
    }

    /// Whether this name is the one the relationship declarations are written under.
    ///
    /// Both spellings, for the reason [`crate::clustering::is_clustering_setting`] takes both. The
    /// caller decides first that no DuckDB setting is called this.
    #[must_use]
    pub fn is_links_setting(name: &str) -> bool {
        name.eq_ignore_ascii_case("graph_links") || name.eq_ignore_ascii_case("graph.links")
    }

    /// Whether the bundled time-zone database knows this name.
    #[must_use]
    pub fn knows_time_zone(name: &str) -> bool {
        SessionTimeZone::named(name).is_some()
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
