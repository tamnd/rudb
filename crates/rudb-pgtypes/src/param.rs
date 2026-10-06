//! The values of the parameters of a `Bind` message, read as rudb values.
//!
//! A client gives each parameter a type OID in `Parse`, or 0 to let the server choose, and sends
//! each value in the text or the binary format. The text format of a type is read with the input
//! function of that type, so `'yes'` is a `boolean` and `'1/2/2024'` is a `date` in the order of
//! `DateStyle`, as in PostgreSQL. A value of an unknown type in the text format is a `VARCHAR`,
//! which the binder casts to the type of the column or the operand it meets, as PostgreSQL does
//! with an `unknown` literal.

use rudb_common::{LogicalType, SqlState, Value, uuid};

use crate::array::{
    Array, array_in, array_recv, int2vector_in, int2vector_recv, oidvector_in, oidvector_recv,
};
use crate::binary::Recv;
use crate::datetime::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateTimeInput, Interval, IntervalStyle,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, UNIX_TO_POSTGRES_DAYS, UNIX_TO_POSTGRES_USECS,
    date_in, date_recv, interval_in, interval_recv, time_in, time_recv, timestamp_in,
    timestamp_recv, timestamptz_in,
};
use crate::declared::declared_type;
use crate::error::TypeError;
use crate::generated::oids;
use crate::number::{int2_in, int4_in, int8_in, oid_in};
use crate::numeric::{Numeric, NumericSign, numeric_in, numeric_out, numeric_recv};
use crate::reg::{RegInput, RegKind, reg_in};
use crate::scalar::{bool_in, bytea_in, char_in, name_in, uuid_in};
use crate::string::{bpchar_in, varchar_in};
use crate::types::{Oid, PgType, TypeInfo, format_type};
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
        oids::OID => LogicalType::UInteger,
        oids::CHAR => LogicalType::UTinyInt,
        oids::INT2VECTOR => LogicalType::List(Box::new(LogicalType::SmallInt)),
        oids::OIDVECTOR => LogicalType::List(Box::new(LogicalType::UInteger)),
        oid if RegKind::from_oid(oid).is_some() => LogicalType::UInteger,
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
        oid => {
            let (element, _) = element(oid)?;
            LogicalType::List(Box::new(logical_type(element)?))
        }
    })
}

/// The element type and the delimiter of an array type whose elements are not arrays.
fn element(oid: Oid) -> Option<(Oid, u8)> {
    let info = TypeInfo::get(oid).filter(|info| info.is_array())?;
    let element = TypeInfo::get(info.elem).filter(|element| !element.is_array())?;
    Some((element.oid, element.delim))
}

/// An array as a rudb list. A list has one dimension and no bounds, so an array with more than one
/// dimension is an error until rudb has them, and the lower bound is not kept.
fn list<T>(element: Oid, array: Array<T>, value: impl Fn(T) -> Value) -> Result<Value, TypeError> {
    if array.dims.len() > 1 {
        return Err(TypeError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "arrays of more than one dimension are not supported".to_owned(),
        ));
    }
    Ok(Value::List {
        element: logical_type(element).unwrap_or(LogicalType::Varchar),
        values: array.values.into_iter().map(|v| v.map_or(Value::Null, &value)).collect(),
    })
}

/// An `int2vector` or an `oidvector` as a rudb list.
fn vector(element: LogicalType, values: impl Iterator<Item = Value>) -> Value {
    Value::List { element, values: values.collect() }
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

/// The value of a column of the type `ty` from one field of `COPY FROM`, in the text format, or in
/// the binary format when `binary` is set. The typmod of the column applies as the input function
/// applies it: a `numeric(p, s)` is rounded to its scale or is an error when it does not fit, and
/// a `varchar(n)` or a `char(n)` that is too long is an error. A `char(n)` value loses its
/// trailing spaces.
///
/// # Errors
///
/// The error of the input or the receive function of the type, with the SQLSTATE and the text of
/// PostgreSQL. A binary value with bytes left over is `incorrect binary data format`.
pub fn column_value(
    ty: PgType,
    binary: bool,
    data: &[u8],
    settings: &InputSettings<'_>,
) -> Result<Value, TypeError> {
    let PgType { oid, typmod } = ty;
    if binary {
        let mut recv = Recv::new(data);
        let value = match oid {
            oids::NUMERIC => numeric(&numeric_recv(&mut recv, typmod)?),
            oids::VARCHAR if typmod >= 0 => {
                Value::Varchar(varchar_in(recv.text()?, typmod)?.to_owned())
            }
            oids::BPCHAR if typmod >= 0 => bpchar_value(recv.text()?, typmod)?,
            _ => binary_value(oid, &mut recv)?,
        };
        if recv.remaining() > 0 {
            return Err(TypeError::new(
                SqlState::INVALID_BINARY_REPRESENTATION,
                "incorrect binary data format".to_owned(),
            ));
        }
        return Ok(value);
    }
    let text = Recv::new(data).text()?;
    Ok(match oid {
        oids::NUMERIC => numeric(&numeric_in(text, typmod)?),
        oids::VARCHAR if typmod >= 0 => Value::Varchar(varchar_in(text, typmod)?.to_owned()),
        oids::BPCHAR if typmod >= 0 => bpchar_value(text, typmod)?,
        _ => text_value(oid, text, settings)?,
    })
}

/// A `char(n)` value as rudb keeps it, with no trailing spaces. The encoder of the rows pads it.
fn bpchar_value(text: &str, typmod: i32) -> Result<Value, TypeError> {
    bpchar_in(text, typmod)?;
    Ok(Value::Varchar(text.trim_end_matches(' ').to_owned()))
}

fn text_value(oid: Oid, text: &str, settings: &InputSettings<'_>) -> Result<Value, TypeError> {
    let cx = &settings.datetime;
    Ok(match oid {
        oids::BOOL => Value::Boolean(bool_in(text)?),
        oids::INT2 => Value::SmallInt(int2_in(text)?),
        oids::INT4 => Value::Integer(int4_in(text)?),
        oids::INT8 => Value::BigInt(int8_in(text)?),
        oids::OID => Value::UInteger(oid_in(text)?),
        oids::CHAR => Value::UTinyInt(char_in(text)),
        oids::INT2VECTOR => {
            let values = int2vector_in(text)?;
            vector(LogicalType::SmallInt, values.into_iter().map(Value::SmallInt))
        }
        oids::OIDVECTOR => {
            let values = oidvector_in(text)?;
            vector(LogicalType::UInteger, values.into_iter().map(Value::UInteger))
        }
        oid if let Some(kind) = RegKind::from_oid(oid) => Value::UInteger(reg_value(kind, text)?),
        oids::NAME => Value::Varchar(name_in(text).to_owned()),
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
        oid if let Some((element, delim)) = element(oid) => {
            let array = array_in(text, delim, true, |text| text_value(element, text, settings))?;
            list(element, array, |value| value)?
        }
        _ => Value::Varchar(text.to_owned()),
    })
}

/// The text input of an OID alias type. A number needs no lookup. A `regtype` name is read as a
/// type name is read in a cast, which finds every built-in type. A name of another kind needs the
/// catalog of PG3.
fn reg_value(kind: RegKind, text: &str) -> Result<u32, TypeError> {
    let name = match reg_in(kind, text)? {
        RegInput::Oid(oid) => return Ok(oid),
        RegInput::Name(name) => name,
    };
    if kind == RegKind::Type {
        // The type grammar reads the SQL names, and `pg_type` has the names such as `_int4`.
        let found = declared_type(name).map(|declared| declared.oid);
        return found.or_else(|| TypeInfo::by_name(name).map(|info| info.oid)).ok_or_else(|| {
            TypeError::new(SqlState::UNDEFINED_OBJECT, format!("type \"{name}\" does not exist"))
        });
    }
    Err(TypeError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("a name as the input of type {} is not supported yet", format_type(kind.oid())),
    ))
}

fn binary_value(oid: Oid, recv: &mut Recv<'_>) -> Result<Value, TypeError> {
    Ok(match oid {
        oids::BOOL => Value::Boolean(recv.bool()?),
        oids::INT2 => Value::SmallInt(recv.i16()?),
        oids::INT4 => Value::Integer(recv.i32()?),
        oids::INT8 => Value::BigInt(recv.i64()?),
        oids::OID => Value::UInteger(recv.u32()?),
        oids::CHAR => Value::UTinyInt(recv.byte()?),
        oids::INT2VECTOR => {
            let values = int2vector_recv(recv)?;
            vector(LogicalType::SmallInt, values.into_iter().map(Value::SmallInt))
        }
        oids::OIDVECTOR => {
            let values = oidvector_recv(recv)?;
            vector(LogicalType::UInteger, values.into_iter().map(Value::UInteger))
        }
        oid if RegKind::from_oid(oid).is_some() => Value::UInteger(recv.u32()?),
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
        oid if let Some((element, _)) = element(oid) => {
            let array = array_recv(recv, element, |recv| binary_value(element, recv))?;
            list(element, array, |value| value)?
        }
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
    fn a_char_column_value_has_no_trailing_spaces() {
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
        let ty = PgType { oid: oids::BPCHAR, typmod: 8 };
        for binary in [false, true] {
            for (data, kept) in [(&b"ab"[..], "ab"), (b"ab  ", "ab"), (b"abcd    ", "abcd")] {
                let value = column_value(ty, binary, data, &settings).unwrap();
                assert_eq!(value, Value::Varchar(kept.into()));
            }
            let error = column_value(ty, binary, b"abcde", &settings).unwrap_err();
            assert_eq!(error.message, "value too long for type character(4)");
        }
    }

    #[test]
    fn an_oid_alias_and_a_vector_read_as_postgresql_reads_them() {
        let oid = |n| Value::UInteger(n);
        assert_eq!(read(oids::REGTYPE, false, b"integer").unwrap(), oid(oids::INT4));
        assert_eq!(read(oids::REGTYPE, false, b"_int4").unwrap(), oid(oids::INT4_ARRAY));
        assert_eq!(read(oids::REGTYPE, false, b"varchar(10)").unwrap(), oid(oids::VARCHAR));
        assert_eq!(read(oids::REGCLASS, false, b"1259").unwrap(), oid(1259));
        assert_eq!(read(oids::REGPROC, false, b"-").unwrap(), oid(0));
        assert_eq!(read(oids::REGTYPE, true, &23u32.to_be_bytes()).unwrap(), oid(23));
        let error = read(oids::REGTYPE, false, b"nope").unwrap_err();
        assert_eq!(error.sqlstate, SqlState::UNDEFINED_OBJECT);
        assert_eq!(error.message, "type \"nope\" does not exist");
        let error = read(oids::REGCLASS, false, b"pg_class").unwrap_err();
        assert_eq!(error.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);

        let list = |element, values| Value::List { element, values };
        let smallints = |values: &[i16]| values.iter().map(|&v| Value::SmallInt(v)).collect();
        assert_eq!(
            read(oids::INT2VECTOR, false, b" 1 2  3").unwrap(),
            list(LogicalType::SmallInt, smallints(&[1, 2, 3]))
        );
        assert_eq!(
            read(oids::OIDVECTOR, false, b"23 25").unwrap(),
            list(LogicalType::UInteger, vec![oid(23), oid(25)])
        );
        let mut bytes = Vec::new();
        crate::array::int2vector_send(&[4, 5], &mut bytes);
        assert_eq!(
            read(oids::INT2VECTOR, true, &bytes).unwrap(),
            list(LogicalType::SmallInt, smallints(&[4, 5]))
        );
        assert_eq!(logical_type(oids::REGTYPE), Some(LogicalType::UInteger));
        assert_eq!(
            logical_type(oids::OIDVECTOR),
            Some(LogicalType::List(Box::new(LogicalType::UInteger)))
        );
    }

    #[test]
    fn the_text_format_uses_the_input_function_of_the_type() {
        assert_eq!(read(oids::BOOL, false, b"yes").unwrap(), Value::Boolean(true));
        assert_eq!(read(oids::INT4, false, b" 42 ").unwrap(), Value::Integer(42));
        assert_eq!(read(oids::OID, false, b"4294967295").unwrap(), Value::UInteger(u32::MAX));
        assert_eq!(read(oids::CHAR, false, b"ab").unwrap(), Value::UTinyInt(b'a'));
        let long = "x".repeat(70);
        assert_eq!(
            read(oids::NAME, false, long.as_bytes()).unwrap(),
            Value::Varchar(long[..63].into())
        );
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
        assert_eq!(
            read(oids::TEXT_ARRAY, false, br#"{a,"b c",NULL}"#).unwrap(),
            Value::List {
                element: LogicalType::Varchar,
                values: vec![Value::Varchar("a".into()), Value::Varchar("b c".into()), Value::Null],
            }
        );
        assert_eq!(
            logical_type(oids::INT8_ARRAY),
            Some(LogicalType::List(Box::new(LogicalType::BigInt)))
        );
        let error = read(oids::INT4_ARRAY, false, b"{{1},{2}}").unwrap_err();
        assert_eq!(error.sqlstate, SqlState::FEATURE_NOT_SUPPORTED);
        let error = read(oids::INT4, false, b"4x").unwrap_err();
        assert_eq!(error.sqlstate, SqlState::INVALID_TEXT_REPRESENTATION);
        assert_eq!(error.message, "invalid input syntax for type integer: \"4x\"");
        let error = read(oids::TEXT, false, b"a\xff").unwrap_err();
        assert_eq!(error.message, "invalid byte sequence for encoding \"UTF8\": 0xff");
    }

    #[test]
    fn the_binary_format_uses_the_receive_function_of_the_type() {
        assert_eq!(read(oids::INT8, true, &7i64.to_be_bytes()).unwrap(), Value::BigInt(7));
        assert_eq!(read(oids::OID, true, &7u32.to_be_bytes()).unwrap(), Value::UInteger(7));
        assert_eq!(read(oids::CHAR, true, b"a").unwrap(), Value::UTinyInt(b'a'));
        assert_eq!(read(oids::DATE, true, &(-10_957i32).to_be_bytes()).unwrap(), Value::Date(0));
        assert_eq!(read(oids::JSONB, true, b"\x01{}").unwrap(), Value::Varchar("{}".into()));
        let error = read(oids::INT4, true, &7i64.to_be_bytes()).unwrap_err();
        assert_eq!(error.sqlstate, SqlState::INVALID_BINARY_REPRESENTATION);
        assert_eq!(error.message, "incorrect binary data format in bind parameter 1");
        let mut array = Vec::new();
        for word in [1, 0, 23, 2, 1, 4, 7, -1] {
            array.extend_from_slice(&i32::to_be_bytes(word));
        }
        let list = Value::List {
            element: LogicalType::Integer,
            values: vec![Value::Integer(7), Value::Null],
        };
        assert_eq!(read(oids::INT4_ARRAY, true, &array).unwrap(), list);
        let error = read(oids::POINT, true, &[0; 16]).unwrap_err();
        assert_eq!(error.message, "no binary input function available for type point");
    }
}
