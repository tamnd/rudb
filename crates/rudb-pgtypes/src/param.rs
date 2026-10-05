//! The values of the parameters of a `Bind` message, read as rudb values.
//!
//! A client gives each parameter a type OID in `Parse`, or 0 to let the server choose, and sends
//! each value in the text or the binary format. The text format of a type is read with the input
//! function of that type, so `'yes'` is a `boolean` and `'1/2/2024'` is a `date` in the order of
//! `DateStyle`, as in PostgreSQL. A value of an unknown type in the text format is a `VARCHAR`,
//! which the binder casts to the type of the column or the operand it meets, as PostgreSQL does
//! with an `unknown` literal.

use rudb_common::{LogicalType, SqlState, Value, uuid};

use crate::binary::Recv;
use crate::datetime::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateTimeInput, Interval, IntervalStyle,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, UNIX_TO_POSTGRES_DAYS, UNIX_TO_POSTGRES_USECS,
    date_in, date_recv, interval_in, interval_recv, time_in, time_recv, timestamp_in,
    timestamp_recv, timestamptz_in,
};
use crate::error::TypeError;
use crate::generated::oids;
use crate::number::{int2_in, int4_in, int8_in};
use crate::numeric::{Numeric, NumericSign, numeric_in, numeric_out, numeric_recv};
use crate::scalar::{bool_in, bytea_in, uuid_in};
use crate::types::{Oid, TypeInfo};
use crate::{float4_in, float8_in};

/// What the text input of a parameter depends on: `DateStyle`, `TimeZone` and `IntervalStyle`.
#[derive(Debug)]
pub struct InputSettings<'a> {
    /// The date and time settings.
    pub datetime: DateTimeInput<'a>,
    /// `IntervalStyle`.
    pub interval_style: IntervalStyle,
}

/// The rudb type that a parameter of the PostgreSQL type `oid` binds as, or `None` for 0, for
/// `unknown` and for a type that rudb does not take as a parameter yet. A parameter with `None`
/// takes its type from where it is written.
#[must_use]
pub fn logical_type(oid: Oid) -> Option<LogicalType> {
    Some(match oid {
        oids::BOOL => LogicalType::Boolean,
        oids::INT2 => LogicalType::SmallInt,
        oids::INT4 => LogicalType::Integer,
        oids::INT8 => LogicalType::BigInt,
        oids::FLOAT4 => LogicalType::Float,
        oids::FLOAT8 => LogicalType::Double,
        oids::TEXT | oids::VARCHAR | oids::BPCHAR | oids::NAME => LogicalType::Varchar,
        oids::BYTEA => LogicalType::Blob,
        oids::UUID => LogicalType::Uuid,
        oids::DATE => LogicalType::Date,
        oids::TIME => LogicalType::Time,
        oids::TIMESTAMP => LogicalType::Timestamp,
        oids::TIMESTAMPTZ => LogicalType::TimestampTz,
        oids::INTERVAL => LogicalType::Interval,
        oids::JSON => LogicalType::Json,
        _ => return None,
    })
}

/// The value of parameter `number`, from 1, of the type `oid` from its bytes in the text format,
/// or in the binary format when `binary` is set.
///
/// # Errors
///
/// The error of the input or the receive function of the type, with the SQLSTATE and the text of
/// PostgreSQL, or `42883` for a binary value of a type that has no receive function here.
pub fn param_value(
    oid: Oid,
    binary: bool,
    data: &[u8],
    number: usize,
    settings: &InputSettings<'_>,
) -> Result<Value, TypeError> {
    if binary {
        let mut recv = Recv::new(data);
        let value = binary_value(oid, &mut recv)?;
        recv.finish(number)?;
        Ok(value)
    } else {
        let text = Recv::new(data).text()?;
        text_value(oid, text, settings)
    }
}

fn text_value(oid: Oid, text: &str, settings: &InputSettings<'_>) -> Result<Value, TypeError> {
    let cx = &settings.datetime;
    Ok(match oid {
        oids::BOOL => Value::Boolean(bool_in(text)?),
        oids::INT2 => Value::SmallInt(int2_in(text)?),
        oids::INT4 => Value::Integer(int4_in(text)?),
        oids::INT8 => Value::BigInt(int8_in(text)?),
        oids::FLOAT4 => Value::Float(float4_in(text)?),
        oids::FLOAT8 => Value::Double(float8_in(text)?),
        oids::NUMERIC => numeric(&numeric_in(text, -1)?),
        oids::BYTEA => Value::Blob(bytea_in(text)?),
        oids::UUID => Value::Uuid(uuid::from_bytes(uuid_in(text)?)),
        oids::DATE => date(date_in(text, cx)?),
        oids::TIME => Value::Time(time_in(text, -1, cx)?),
        oids::TIMESTAMP => Value::Timestamp(timestamp(timestamp_in(text, -1, cx)?)),
        oids::TIMESTAMPTZ => Value::TimestampTz(timestamp(timestamptz_in(text, -1, cx)?)),
        oids::INTERVAL => interval(interval_in(text, -1, settings.interval_style)?),
        _ => Value::Varchar(text.to_owned()),
    })
}

fn binary_value(oid: Oid, recv: &mut Recv<'_>) -> Result<Value, TypeError> {
    Ok(match oid {
        oids::BOOL => Value::Boolean(recv.bool()?),
        oids::INT2 => Value::SmallInt(recv.i16()?),
        oids::INT4 => Value::Integer(recv.i32()?),
        oids::INT8 => Value::BigInt(recv.i64()?),
        oids::FLOAT4 => Value::Float(recv.f32()?),
        oids::FLOAT8 => Value::Double(recv.f64()?),
        oids::NUMERIC => numeric(&numeric_recv(recv, -1)?),
        oids::BYTEA => Value::Blob(recv.rest().to_vec()),
        oids::TEXT | oids::VARCHAR | oids::BPCHAR | oids::NAME | oids::UNKNOWN | oids::JSON => {
            Value::Varchar(recv.text()?.to_owned())
        }
        oids::JSONB => {
            let version = recv.byte()?;
            if version != 1 {
                return Err(TypeError::new(
                    SqlState::INVALID_BINARY_REPRESENTATION,
                    format!("unsupported jsonb version number {version}"),
                ));
            }
            Value::Varchar(recv.text()?.to_owned())
        }
        oids::UUID => Value::Uuid(uuid::from_bytes(recv.uuid()?)),
        oids::DATE => date(date_recv(recv)?),
        oids::TIME => Value::Time(time_recv(recv, -1)?),
        oids::TIMESTAMP => Value::Timestamp(timestamp(timestamp_recv(recv, -1)?)),
        oids::TIMESTAMPTZ => Value::TimestampTz(timestamp(timestamp_recv(recv, -1)?)),
        oids::INTERVAL => interval(interval_recv(recv, -1)?),
        _ => {
            let name = TypeInfo::get(oid).map_or_else(|| oid.to_string(), |info| info.name.into());
            return Err(TypeError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("no binary input function available for type {name}"),
            ));
        }
    })
}

/// A `numeric` as a `DECIMAL(38, s)` when it fits, and as a `DOUBLE` when it does not or when
/// it is `NaN` or an infinity.
fn numeric(value: &Numeric) -> Value {
    let scale = value.dscale();
    if value.sign() != NumericSign::NaN
        && let Ok(scale) = u8::try_from(scale)
        && scale <= 38
        && let Some(unscaled) = value.to_decimal(u32::from(scale))
        && unscaled.unsigned_abs() < 10u128.pow(38)
    {
        return Value::Decimal { unscaled, width: 38, scale };
    }
    let mut text = Vec::new();
    numeric_out(value, &mut text);
    Value::Double(
        std::str::from_utf8(&text).ok().and_then(|text| text.parse().ok()).unwrap_or(f64::NAN),
    )
}

/// A date of PostgreSQL as rudb counts it, with the infinities of rudb.
fn date(days: i32) -> Value {
    Value::Date(match days {
        DATE_INFINITY => i32::MAX,
        DATE_NEGATIVE_INFINITY => -i32::MAX,
        days => days - UNIX_TO_POSTGRES_DAYS,
    })
}

/// A timestamp of PostgreSQL as rudb counts it, with the infinities of rudb.
fn timestamp(micros: i64) -> i64 {
    match micros {
        TIMESTAMP_INFINITY => i64::MAX,
        TIMESTAMP_NEGATIVE_INFINITY => -i64::MAX,
        micros => micros - UNIX_TO_POSTGRES_USECS,
    }
}

fn interval(value: Interval) -> Value {
    Value::Interval { months: value.month, days: value.day, micros: value.time }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::{DateOrder, FixedZone, NoZones, ZoneAbbrevs};

    fn read(oid: Oid, binary: bool, data: &[u8]) -> Result<Value, TypeError> {
        let zone = FixedZone::utc();
        let settings = InputSettings {
            datetime: DateTimeInput {
                order: DateOrder::Mdy,
                zone: &zone,
                zones: &NoZones,
                abbrevs: ZoneAbbrevs::postgres_default(),
                now: 0,
            },
            interval_style: IntervalStyle::Postgres,
        };
        param_value(oid, binary, data, 1, &settings)
    }

    #[test]
    fn the_text_format_uses_the_input_function_of_the_type() {
        assert_eq!(read(oids::BOOL, false, b"yes").unwrap(), Value::Boolean(true));
        assert_eq!(read(oids::INT4, false, b" 42 ").unwrap(), Value::Integer(42));
        assert_eq!(read(oids::DATE, false, b"1/2/1970").unwrap(), Value::Date(1));
        assert_eq!(read(oids::DATE, false, b"infinity").unwrap(), Value::Date(i32::MAX));
        assert_eq!(
            read(oids::TIMESTAMP, false, b"1970-01-01 00:00:01").unwrap(),
            Value::Timestamp(1_000_000)
        );
        assert_eq!(
            read(oids::NUMERIC, false, b"12.340").unwrap(),
            Value::Decimal { unscaled: 12340, width: 38, scale: 3 }
        );
        assert!(
            matches!(read(oids::NUMERIC, false, b"NaN").unwrap(), Value::Double(v) if v.is_nan())
        );
        assert_eq!(read(0, false, b"abc").unwrap(), Value::Varchar("abc".into()));
        let error = read(oids::INT4, false, b"4x").unwrap_err();
        assert_eq!(error.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(error.message, "invalid input syntax for type integer: \"4x\"");
        let error = read(oids::TEXT, false, b"a\xff").unwrap_err();
        assert_eq!(error.message, "invalid byte sequence for encoding \"UTF8\": 0xff");
    }

    #[test]
    fn the_binary_format_uses_the_receive_function_of_the_type() {
        assert_eq!(read(oids::INT8, true, &7i64.to_be_bytes()).unwrap(), Value::BigInt(7));
        assert_eq!(read(oids::DATE, true, &(-10_957i32).to_be_bytes()).unwrap(), Value::Date(0));
        assert_eq!(read(oids::JSONB, true, b"\x01{}").unwrap(), Value::Varchar("{}".into()));
        let error = read(oids::INT4, true, &7i64.to_be_bytes()).unwrap_err();
        assert_eq!(error.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        assert_eq!(error.message, "incorrect binary data format in bind parameter 1");
        let error = read(oids::POINT, true, &[0; 16]).unwrap_err();
        assert_eq!(error.message, "no binary input function available for type point");
    }
}
