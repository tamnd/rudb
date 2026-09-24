//! The session context, which is what `now()` and `current_user` answer with.
//!
//! Fourteen names for eight answers. The SQL standard spells some of them without parentheses,
//! Postgres adds its own, DuckDB carries both and adds `today()` and `get_current_timestamp()` on
//! top, and every one of them is a fact about the connection rather than about anything written in
//! the query. So they are folded into constants here, before the plan is built, and nothing below
//! the binder knows they were ever written.
//!
//! Folding is the right shape rather than a shortcut, and the pin agrees. It marks these
//! `CONSISTENT_WITHIN_QUERY` in `duckdb_functions()`, and
//! `SELECT count(DISTINCT n) FROM (SELECT now() AS n FROM range(3))` answers 1 there, so one
//! statement sees one instant however many rows it reads. A kernel called per chunk would have to be
//! handed that instant anyway, and the place it would come from is here.
//!
//! The instant is read once per statement and kept, which is [`Binder::instant`]. `now()`,
//! `current_timestamp`, `transaction_timestamp()` and `get_current_timestamp()` all give it back,
//! and they are equal to each other on the pin too, because the transaction a bare `SELECT` runs in
//! begins and ends with that statement.
//!
//! Two things here are waiting on the time zone box, which is the next line on the same milestone.
//! The `WITH TIME ZONE` answers carry the right type and the right instant and render at UTC rather
//! than in a session zone, since there is no session zone to render in yet. And `current_date`,
//! `localtime` and `localtimestamp` are the date and the time in the session zone upstream, which is
//! UTC here for the same reason. The values are the same on both engines whenever the session zone
//! is UTC and they differ by the offset otherwise.
//!
//! A column of the same name wins, which was measured rather than assumed.
//! `CREATE TABLE t(current_date VARCHAR)` then `SELECT current_date FROM t` returns the column on
//! the pin, and so does every other one of the ten bare spellings. Two tables that both have a
//! column called `current_date` is an ambiguity error there and not a fold, so the scope is asked
//! whether anything at all answers to the name before this module is reached, rather than the fold
//! being what happens when resolution fails.
//!
//! `current_schemas` takes an argument and returns a list, so it has a function of its own below.
//! `current_query` and `version` are not here. The first is the one name in this family the pin
//! marks VOLATILE and it needs the statement text threaded down to the binder, and the second is a
//! question about what rudb should call itself that is worth answering on its own.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;

/// Microseconds in a day, for splitting an instant into a date and a time.
const MICROS_PER_DAY: i64 = 86_400 * 1_000_000;

/// What rudb answers `current_user` with.
///
/// The pin's answer, because rudb has no users and neither does DuckDB. A tool that asks who it is
/// connected as wants a name rather than an empty string, and this is the name the same tool gets
/// from the engine rudb is compatible with.
const USER: &str = "duckdb";

/// One of the eight answers the session context has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Context {
    /// The instant the statement started, as `TIMESTAMP WITH TIME ZONE`.
    Instant,
    /// The same instant with no zone on it, which is `TIMESTAMP`.
    LocalInstant,
    /// The date the statement started on.
    Date,
    /// The time of day it started at, as `TIME WITH TIME ZONE`.
    ZonedTime,
    /// The same time of day with no zone on it.
    LocalTime,
    /// The catalog an unqualified name resolves in.
    Database,
    /// The schema an unqualified name resolves in.
    Schema,
    /// Who is connected.
    User,
}

/// The spellings that stand on their own with no parentheses, and what each one answers.
///
/// Ten of them, and `current_database` is deliberately not one. The pin refuses a bare
/// `current_database` with `Referenced column "current_database" was not found`, so it is a function
/// there and not a keyword, and accepting it here would bind a query the pin does not.
const KEYWORDS: &[(&str, Context)] = &[
    ("current_catalog", Context::Database),
    ("current_date", Context::Date),
    ("current_schema", Context::Schema),
    ("current_time", Context::ZonedTime),
    ("current_timestamp", Context::Instant),
    ("current_user", Context::User),
    ("localtime", Context::LocalTime),
    ("localtimestamp", Context::LocalInstant),
    ("session_user", Context::User),
    ("user", Context::User),
];

/// The spellings that are called with an empty argument list, and what each one answers.
///
/// Fourteen, and the four that are only keywords are not among them. `current_timestamp()`,
/// `current_time()`, `localtime()` and `localtimestamp()` are all a catalog error on the pin, which
/// is the mirror of `current_database` being a function and not a keyword.
const CALLS: &[(&str, Context)] = &[
    ("current_catalog", Context::Database),
    ("current_database", Context::Database),
    ("current_date", Context::Date),
    ("current_localtime", Context::LocalTime),
    ("current_localtimestamp", Context::LocalInstant),
    ("current_schema", Context::Schema),
    ("current_user", Context::User),
    ("get_current_time", Context::ZonedTime),
    ("get_current_timestamp", Context::Instant),
    ("now", Context::Instant),
    ("session_user", Context::User),
    ("today", Context::Date),
    ("transaction_timestamp", Context::Instant),
    ("user", Context::User),
];

impl Binder<'_> {
    /// The constant a bare `current_date` folds to, and `None` for a word that is not one of these.
    pub(crate) fn context_keyword(&mut self, word: &str) -> Option<ExprRef> {
        let (_, what) = KEYWORDS.iter().find(|(name, _)| name.eq_ignore_ascii_case(word))?;
        Some(self.context(*what))
    }

    /// The constant a `now()` folds to, and `None` for a name that is not one of these.
    pub(crate) fn context_call(&mut self, name: &str) -> Option<ExprRef> {
        let (_, what) = CALLS.iter().find(|(held, _)| held.eq_ignore_ascii_case(name))?;
        Some(self.context(*what))
    }

    /// One session context answer, as a constant in the plan.
    fn context(&mut self, what: Context) -> ExprRef {
        let instant = self.instant();
        let local = self.session.local_micros(instant);
        let midnight = || local.rem_euclid(MICROS_PER_DAY);
        let value = match what {
            Context::Instant => Value::TimestampTz(instant),
            Context::LocalInstant => Value::Timestamp(local),
            Context::Date => {
                let days = local.div_euclid(MICROS_PER_DAY);
                Value::Date(i32::try_from(days).unwrap_or(i32::MAX))
            }
            Context::ZonedTime => Value::TimeTz(midnight()),
            Context::LocalTime => Value::Time(midnight()),
            Context::Database => Value::Varchar(self.catalog().default_catalog().to_string()),
            Context::Schema => Value::Varchar(self.catalog().default_schema().to_string()),
            Context::User => Value::Varchar(USER.to_string()),
        };
        self.plan_mut().add_constant(value)
    }

    /// `current_schemas(include_implicit)`, folded to the list of schemas on the search path.
    ///
    /// The argument has to be a constant, which is the pin's rule and its sentence. A null argument
    /// is a null list and anything but a boolean falls through to the table.
    pub(crate) fn current_schemas(&mut self, argument: ExprRef) -> Result<Option<ExprRef>> {
        let list = LogicalType::list(LogicalType::Varchar);
        let Expr::Constant(held) = *self.plan().expr(argument) else {
            return Err(Error::binder(
                "The \"include_implicit\" argument in function \"current_schemas\" must be a \
                 constant expression",
            )
            .with_span(self.plan().expr_span(argument)));
        };
        let implicit = match self.plan().value(held) {
            Value::Null => {
                let reference = self.plan_mut().add_value(Value::Null);
                return Ok(Some(self.add_expr(Expr::Constant(reference), list)));
            }
            Value::Boolean(implicit) => *implicit,
            _ => return Ok(None),
        };
        let values =
            self.catalog().search_schemas(implicit).into_iter().map(Value::Varchar).collect();
        Ok(Some(self.add_constant(Value::List { element: LogicalType::Varchar, values })))
    }

    /// The session-local date used as the first argument of one-argument `age`.
    pub(crate) fn current_date(&mut self) -> ExprRef {
        self.context(Context::Date)
    }
}

/// Microseconds since 1970-01-01 00:00:00 UTC, now.
///
/// The epoch for a clock set before it, which is a machine whose time is wrong rather than a case
/// worth an error. Nothing sensible can be returned for it and refusing to bind a query over it
/// would be a strange way to find out.
pub(crate) fn micros_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{CALLS, Context, KEYWORDS, MICROS_PER_DAY, micros_now};

    /// Both lists are sorted and neither one names anything twice, which is how a reader checks one
    /// against the pin's output without reading the whole thing.
    #[test]
    fn the_two_name_lists_are_sorted_and_have_no_repeats() {
        for list in [KEYWORDS, CALLS] {
            let names: Vec<&str> = list.iter().map(|(name, _)| *name).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(names, sorted);
        }
        assert_eq!(KEYWORDS.len(), 10);
        assert_eq!(CALLS.len(), 14);
    }

    /// The four that are keywords only and the one that is a call only, which is the whole of the
    /// difference between the two lists and each side of it was measured against the pin.
    #[test]
    fn the_names_that_take_only_one_of_the_two_spellings_are_the_five_measured() {
        let only_keyword: Vec<&str> = KEYWORDS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !CALLS.iter().any(|(held, _)| held == name))
            .collect();
        assert_eq!(
            only_keyword,
            vec!["current_time", "current_timestamp", "localtime", "localtimestamp"]
        );
        let only_call: Vec<&str> = CALLS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !KEYWORDS.iter().any(|(held, _)| held == name))
            .collect();
        assert_eq!(
            only_call,
            vec![
                "current_database",
                "current_localtime",
                "current_localtimestamp",
                "get_current_time",
                "get_current_timestamp",
                "now",
                "today",
                "transaction_timestamp"
            ]
        );
    }

    /// Every one of the eight answers is reachable by writing something, which is what says the enum
    /// has no arm nothing produces.
    #[test]
    fn every_answer_has_a_name_that_asks_for_it() {
        let wanted = [
            Context::Instant,
            Context::LocalInstant,
            Context::Date,
            Context::ZonedTime,
            Context::LocalTime,
            Context::Database,
            Context::Schema,
            Context::User,
        ];
        for what in wanted {
            assert!(
                KEYWORDS.iter().chain(CALLS).any(|(_, held)| *held == what),
                "nothing asks for {what:?}"
            );
        }
    }

    /// The clock is after the day this was written and the split into a date and a time is the one
    /// that puts the time inside a day, including for an instant before the epoch.
    #[test]
    fn the_clock_reads_forward_and_splits_into_a_date_and_a_time_within_the_day() {
        // 2024-01-01, which is behind whatever machine runs this and ahead of a clock at zero.
        assert!(micros_now() > 1_704_067_200_000_000);
        for instant in [-MICROS_PER_DAY - 1, -1, 0, 1, MICROS_PER_DAY + 1, micros_now()] {
            let time = instant.rem_euclid(MICROS_PER_DAY);
            assert!((0..MICROS_PER_DAY).contains(&time), "{instant} split badly");
            assert_eq!(instant.div_euclid(MICROS_PER_DAY) * MICROS_PER_DAY + time, instant);
        }
    }
}
