//! `pg_input_is_valid` and `pg_input_error_info` of a PostgreSQL session, which read a string with
//! the input function of a type and catch its error.
//!
//! The binder gives each call the `DateStyle` and the `IntervalStyle` of the session as two more
//! arguments. The zone is UTC, since the zone changes the value that a `timestamptz` reads and not
//! whether the text is one.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_pgtypes::{
    DateFormat, DateTimeInput, FixedZone, InputSettings, IntervalStyle, SessionZones, TypeError,
    ZoneAbbrevs,
};

/// The fields of the record of `pg_input_error_info`.
pub const ERROR_FIELDS: [&str; 4] = ["message", "detail", "hint", "sql_error_code"];

/// The type of the record of `pg_input_error_info`.
#[must_use]
pub fn error_type() -> LogicalType {
    let fields =
        ERROR_FIELDS.iter().map(|name| rudb_common::Field::new(*name, LogicalType::Varchar));
    LogicalType::Struct(fields.collect())
}

/// `__rudb_pg_input_valid(text, type, DateStyle, IntervalStyle)` and
/// `__rudb_pg_input_error` with the same arguments, or `None` for another function.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let valid = match name {
        "__rudb_pg_input_valid" => true,
        "__rudb_pg_input_error" => false,
        _ => return Ok(None),
    };
    let [text, type_name, date_style, interval_style] = args else {
        return Err(Error::internal(format!("{name} takes 4 arguments")));
    };
    let (Value::Varchar(text), Value::Varchar(type_name)) = (text, type_name) else {
        // Both functions are strict.
        return Ok(Some(Value::Null));
    };
    let setting = |value: &Value| match value {
        Value::Varchar(text) => text.clone(),
        _ => String::new(),
    };
    let zone = FixedZone::utc();
    let settings = InputSettings {
        datetime: DateTimeInput {
            order: DateFormat::of_setting(&setting(date_style)).order,
            zone: &zone,
            zones: &SessionZones,
            abbrevs: ZoneAbbrevs::postgres_default(),
            now: 0,
        },
        interval_style: IntervalStyle::of_setting(&setting(interval_style)),
    };
    let error = rudb_pgtypes::soft_input_error(text, type_name, &settings)
        .map_err(|error| Error::from(error).unplaced())?;
    Ok(Some(if valid { Value::Boolean(error.is_none()) } else { error_record(error) }))
}

/// The record of `pg_input_error_info`, with every field null for a valid text.
fn error_record(error: Option<TypeError>) -> Value {
    let (message, detail, hint, code) = match error {
        Some(error) => (
            Some(error.message),
            error.detail,
            error.hint,
            Some(error.sqlstate.as_str().to_owned()),
        ),
        None => (None, None, None, None),
    };
    let field = |value: Option<String>| value.map_or(Value::Null, Value::Varchar);
    let values = [field(message), field(detail), field(hint), field(code)];
    Value::Struct(ERROR_FIELDS.iter().map(|name| (*name).to_owned()).zip(values).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudb_common::SqlState;

    fn call_with(name: &str, text: &str, type_name: &str, date_style: &str) -> Result<Value> {
        let string = |text: &str| Value::Varchar(text.to_owned());
        let args = [string(text), string(type_name), string(date_style), string("postgres")];
        Ok(call(name, &args)?.unwrap())
    }

    #[test]
    fn the_input_functions_catch_the_error_of_the_text() {
        let valid =
            |text, type_name, style| call_with("__rudb_pg_input_valid", text, type_name, style);
        assert_eq!(valid("12", "int4", "ISO, MDY").unwrap(), Value::Boolean(true));
        assert_eq!(valid("x", "int4", "ISO, MDY").unwrap(), Value::Boolean(false));
        assert_eq!(valid("13/01/2024", "date", "ISO, MDY").unwrap(), Value::Boolean(false));
        assert_eq!(valid("13/01/2024", "date", "ISO, DMY").unwrap(), Value::Boolean(true));
        let missing = valid("1", "nosuch", "ISO, MDY").unwrap_err();
        assert_eq!(missing.sqlstate(), Some(SqlState::UNDEFINED_OBJECT));
        let info = call_with("__rudb_pg_input_error", "x", "int4", "ISO, MDY").unwrap();
        let Value::Struct(fields) = info else { panic!("{info:?}") };
        assert_eq!(
            fields[0].1,
            Value::Varchar("invalid input syntax for type integer: \"x\"".into())
        );
        assert_eq!((&fields[1].1, &fields[2].1), (&Value::Null, &Value::Null));
        assert_eq!(fields[3].1, Value::Varchar("22P02".into()));
        let args = [Value::Null, Value::Varchar("int4".into()), Value::Null, Value::Null];
        assert_eq!(call("__rudb_pg_input_valid", &args).unwrap(), Some(Value::Null));
    }
}
