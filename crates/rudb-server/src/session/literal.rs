//! The input functions of the types for a string literal in a cast, such as `'\x01ff'::bytea` or
//! `'infinity'::date`. The engine casts text in the way of DuckDB, which does not read these forms.
//! An error of an input function read here has the place of the literal, as in PostgreSQL.

use std::time::{SystemTime, UNIX_EPOCH};

use rudb_common::session::LiteralInput;
use rudb_common::value::Value;
use rudb_common::{Error, Result};
use rudb_pgtypes::{
    DateOrder, DateTimeInput, InputSettings, IntervalStyle, NoZones, RegKind, TypeInfo,
    UNIX_TO_POSTGRES_USECS, ZoneAbbrevs, json_in, jsonb_in, numeric_in, oid, param_value,
};

use super::zone;

/// The settings that the input functions read: `DateStyle`, `TimeZone` and `IntervalStyle`.
#[derive(Debug)]
pub(super) struct Literals {
    pub(super) order: DateOrder,
    pub(super) zone: String,
    pub(super) interval_style: IntervalStyle,
}

impl LiteralInput for Literals {
    fn read(&self, oid: u32, text: &str) -> Option<Result<Value>> {
        // A `numeric` keeps all its digits, where a parameter of `param_value` with more than 38
        // digits is a double. A `json` is kept as it was written, and a `jsonb` in its normal form.
        let read = match oid {
            oid::NUMERIC => numeric_in(text, -1).map(|value| Value::Numeric(value.to_bytes())),
            oid::JSON => json_in(text).map(|text| Value::Varchar(text.to_owned())),
            oid::JSONB => jsonb_in(text).map(Value::Varchar),
            _ => return self.read_known(oid, text),
        };
        Some(read.map_err(Error::from))
    }
}

impl Literals {
    /// The types whose input is read by [`param_value`].
    fn read_known(&self, oid: u32, text: &str) -> Option<Result<Value>> {
        let known = [
            oid::BOOL,
            oid::CHAR,
            oid::NAME,
            oid::INT2,
            oid::INT4,
            oid::INT8,
            oid::OID,
            oid::FLOAT4,
            oid::FLOAT8,
            oid::BYTEA,
            oid::UUID,
            oid::DATE,
            oid::TIME,
            oid::TIMESTAMP,
            oid::TIMESTAMPTZ,
            oid::INTERVAL,
            oid::INT2VECTOR,
            oid::OIDVECTOR,
        ];
        // An array of a known type, such as `'{1,2}'::int4[]`, is read by the array input.
        let element = TypeInfo::get(oid).filter(|info| info.is_array()).map(|info| info.elem);
        let strings = [oid::TEXT, oid::VARCHAR];
        let reg = |oid| RegKind::from_oid(oid).is_some();
        let known_element =
            |element| known.contains(&element) || strings.contains(&element) || reg(element);
        if !known.contains(&oid) && !reg(oid) && !element.is_some_and(known_element) {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
        let zone = zone::of(&self.zone);
        let settings = InputSettings {
            datetime: DateTimeInput {
                order: self.order,
                zone: &zone,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: now + UNIX_TO_POSTGRES_USECS,
            },
            interval_style: self.interval_style,
        };
        Some(param_value(oid, false, text.as_bytes(), 0, &settings).map_err(Error::from))
    }
}
