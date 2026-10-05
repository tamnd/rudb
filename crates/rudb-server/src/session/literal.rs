//! The input functions of the types for a string literal in a cast, such as `'\x01ff'::bytea` or
//! `'infinity'::date`. The engine casts text in the way of DuckDB, which does not read these forms.

use std::time::{SystemTime, UNIX_EPOCH};

use rudb_common::session::LiteralInput;
use rudb_common::types::LogicalType;
use rudb_common::value::Value;
use rudb_common::{Error, Result};
use rudb_pgtypes::{
    DateOrder, DateTimeInput, InputSettings, IntervalStyle, NoZones, UNIX_TO_POSTGRES_USECS,
    ZoneAbbrevs, oid, param_value,
};

use super::zone::Zone;

/// The settings that the input functions read: `DateStyle`, `TimeZone` and `IntervalStyle`.
#[derive(Debug)]
pub(super) struct Literals {
    pub(super) order: DateOrder,
    pub(super) zone: String,
    pub(super) interval_style: IntervalStyle,
}

impl LiteralInput for Literals {
    fn read(&self, ty: &LogicalType, text: &str) -> Option<Result<Value>> {
        let oid = match ty {
            LogicalType::Boolean => oid::BOOL,
            LogicalType::SmallInt => oid::INT2,
            LogicalType::Integer => oid::INT4,
            LogicalType::BigInt => oid::INT8,
            LogicalType::Float => oid::FLOAT4,
            LogicalType::Double => oid::FLOAT8,
            LogicalType::Blob => oid::BYTEA,
            LogicalType::Uuid => oid::UUID,
            LogicalType::Date => oid::DATE,
            LogicalType::Time => oid::TIME,
            LogicalType::Timestamp => oid::TIMESTAMP,
            LogicalType::TimestampTz => oid::TIMESTAMPTZ,
            LogicalType::Interval => oid::INTERVAL,
            _ => return None,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
        let zone = Zone::of(&self.zone);
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
        Some(param_value(oid, false, text.as_bytes(), 0, &settings).map_err(|error| {
            let mut out = Error::conversion(error.message).state(error.sqlstate);
            if let Some(detail) = error.detail {
                out = out.detail(detail);
            }
            if let Some(hint) = error.hint {
                out = out.hint(hint);
            }
            out
        }))
    }
}
