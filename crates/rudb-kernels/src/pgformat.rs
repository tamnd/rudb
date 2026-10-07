//! `to_char`, `to_timestamp` and `to_date` of a PostgreSQL session, over the ports of
//! `formatting.c` in `rudb_pgtypes`.
//!
//! The binder writes `to_char` over a `timestamp`, a `timestamptz`, an `interval` or a `time` as
//! `__rudb_pg_to_char`, `to_timestamp(text, text)` as `__rudb_pg_to_timestamp` and
//! `to_date(text, text)` as `__rudb_pg_to_date`. A `timestamptz` shows the wall clock of the
//! session zone, and a string with no zone in it is read in the session zone, so the calls take the
//! zone of the session. A `time` is formatted as the interval that PostgreSQL casts it to.
//!
//! The template is parsed once for each run of rows that have the same template, which is once
//! for a constant template.

use rudb_common::{Error, LogicalType, Result, SessionTimeZone, Value};
use rudb_pgtypes::{
    DateOrder, DateTemplate, DateTimeInput, Interval, SessionZones, UNIX_TO_POSTGRES_DAYS,
    ZoneAbbrevs, interval_to_char, timestamp_from_unix, timestamp_to_char, timestamp_to_unix,
    timestamptz_to_char, to_date, to_timestamp,
};
use rudb_vector::Vector;

/// Whether `name` is one of the calls of this module.
pub(crate) fn formats(name: &str) -> bool {
    matches!(name, "__rudb_pg_to_char" | "__rudb_pg_to_timestamp" | "__rudb_pg_to_date")
}

/// The answers of a call over vectors, with the zone of the session.
pub(crate) fn call_vectors(
    name: &str,
    args: &[&Vector],
    returns: &LogicalType,
    zone: SessionTimeZone,
) -> Result<Vector> {
    let [value, template] = args else {
        return Err(Error::internal(format!("{name} takes a value and a template")));
    };
    let mut formatter = Formatter { zone, parsed: None };
    let mut answers = Vec::with_capacity(value.len());
    for at in 0..value.len() {
        answers.push(formatter.one(name, &value.try_value_at(at)?, &template.try_value_at(at)?)?);
    }
    Vector::from_values(returns.clone(), &answers)
}

/// The answer of a call over values, or `None` when `name` is not one of the calls of this module.
pub(crate) fn call_value(
    name: &str,
    args: &[Value],
    zone: SessionTimeZone,
) -> Result<Option<Value>> {
    if !formats(name) {
        return Ok(None);
    }
    let [value, template] = args else {
        return Err(Error::internal(format!("{name} takes a value and a template")));
    };
    Formatter { zone, parsed: None }.one(name, value, template).map(Some)
}

/// The session zone and the last template that was parsed, with its text.
struct Formatter {
    zone: SessionTimeZone,
    parsed: Option<(String, DateTemplate)>,
}

impl Formatter {
    fn template(&mut self, text: &str) -> &DateTemplate {
        if self.parsed.as_ref().is_none_or(|(kept, _)| kept != text) {
            self.parsed = Some((text.to_owned(), DateTemplate::parse(text)));
        }
        &self.parsed.as_ref().expect("the template was parsed above").1
    }

    fn one(&mut self, name: &str, value: &Value, template: &Value) -> Result<Value> {
        let (Value::Varchar(text), false) = (template, value.is_null()) else {
            return Ok(Value::Null);
        };
        let zone = self.zone;
        let template = self.template(text);
        // PostgreSQL places no error of a value that it reads as the query runs.
        let placed = |error| Error::from(error).unplaced();
        match (name, value) {
            ("__rudb_pg_to_char", value) => {
                let written = match value {
                    Value::Timestamp(micros) => {
                        timestamp_to_char(timestamp_from_unix(*micros).map_err(placed)?, template)
                    }
                    Value::TimestampTz(micros) => timestamptz_to_char(
                        timestamp_from_unix(*micros).map_err(placed)?,
                        &zone,
                        template,
                    ),
                    Value::Interval { months, days, micros } => interval_to_char(
                        &Interval { time: *micros, day: *days, month: *months },
                        template,
                    ),
                    Value::Time(micros) => {
                        interval_to_char(&Interval { time: *micros, day: 0, month: 0 }, template)
                    }
                    other => {
                        return Err(Error::internal(format!(
                            "to_char over a {}",
                            other.logical_type()
                        )));
                    }
                };
                Ok(written.map_err(placed)?.map_or(Value::Null, Value::Varchar))
            }
            ("__rudb_pg_to_timestamp", Value::Varchar(input)) => {
                let ts = to_timestamp(input, template, &input_context(&zone)).map_err(placed)?;
                Ok(Value::TimestampTz(timestamp_to_unix(ts).map_err(placed)?))
            }
            ("__rudb_pg_to_date", Value::Varchar(input)) => {
                let date = to_date(input, template, &input_context(&zone)).map_err(placed)?;
                Ok(Value::Date(date - UNIX_TO_POSTGRES_DAYS))
            }
            (name, other) => {
                Err(Error::internal(format!("{name} over a {}", other.logical_type())))
            }
        }
    }
}

/// What the input of a date reads in the session. The template gives the order of the fields
/// and no field reads the current time, so the order and the time are not the ones of the
/// session.
fn input_context(zone: &SessionTimeZone) -> DateTimeInput<'_> {
    DateTimeInput {
        order: DateOrder::Mdy,
        zone,
        zones: &SessionZones,
        abbrevs: ZoneAbbrevs::postgres_default(),
        now: 0,
    }
}
