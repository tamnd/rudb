//! Values of `VARIANT`, a value of any type that carries its type with it.
//!
//! A variant is held as bytes that describe themselves: a byte that says the kind, which is the
//! pin's `VariantLogicalType` number, and then the payload of that kind. Numbers and times are
//! their fixed width little endian bytes, a decimal is its width, its scale and its unscaled
//! sixteen bytes, and a string, a blob, a bit string or a bignum is a four byte length and then
//! the bytes. An array is a four byte count and then the children one after another, and an object
//! is a four byte count and then, for each key in byte order, the key as a length and its bytes
//! followed by the child. A null inside an array or an object is the null kind with no payload,
//! and a variant that is itself null is a SQL null and is never held here.
//!
//! The bytes do not order or group the way the values do, since `1` and `1.0` are one value to the
//! pin and two kinds here. [`sort_key`] is what does, and it is byte for byte the pin's
//! `variant_comparator`.

use crate::types::LogicalType;
use crate::value::Value;

/// The kinds, numbered as the pin's `VariantLogicalType` is.
pub mod kind {
    pub const NULL: u8 = 0;
    pub const TRUE: u8 = 1;
    pub const FALSE: u8 = 2;
    pub const INT8: u8 = 3;
    pub const INT16: u8 = 4;
    pub const INT32: u8 = 5;
    pub const INT64: u8 = 6;
    pub const INT128: u8 = 7;
    pub const UINT8: u8 = 8;
    pub const UINT16: u8 = 9;
    pub const UINT32: u8 = 10;
    pub const UINT64: u8 = 11;
    pub const UINT128: u8 = 12;
    pub const FLOAT: u8 = 13;
    pub const DOUBLE: u8 = 14;
    pub const DECIMAL: u8 = 15;
    pub const VARCHAR: u8 = 16;
    pub const BLOB: u8 = 17;
    pub const UUID: u8 = 18;
    pub const DATE: u8 = 19;
    pub const TIME_MICROS: u8 = 20;
    pub const TIME_NANOS: u8 = 21;
    pub const TIMESTAMP_SEC: u8 = 22;
    pub const TIMESTAMP_MILIS: u8 = 23;
    pub const TIMESTAMP_MICROS: u8 = 24;
    pub const TIMESTAMP_NANOS: u8 = 25;
    pub const TIME_MICROS_TZ: u8 = 26;
    pub const TIMESTAMP_MICROS_TZ: u8 = 27;
    pub const INTERVAL: u8 = 28;
    pub const OBJECT: u8 = 29;
    pub const ARRAY: u8 = 30;
    pub const BIGNUM: u8 = 31;
    pub const BITSTRING: u8 = 32;
    pub const GEOMETRY: u8 = 33;
    pub const TIMESTAMP_NANOS_TZ: u8 = 34;
}

/// The variant a value is, or `None` for a SQL null, which stays one.
#[must_use]
pub fn encode(value: &Value) -> Option<Vec<u8>> {
    if value.is_null() {
        return None;
    }
    if let Value::Variant(held) = value {
        return Some(held.clone());
    }
    let mut out = Vec::new();
    write(value, &mut out);
    Some(out)
}

/// Appends a value as a variant. A null is the null kind, since this is only ever asked of a
/// value at the top by [`encode`], which has already kept a null out.
pub fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(kind::NULL),
        Value::Boolean(held) => out.push(if *held { kind::TRUE } else { kind::FALSE }),
        Value::TinyInt(held) => fixed(out, kind::INT8, &held.to_le_bytes()),
        Value::SmallInt(held) => fixed(out, kind::INT16, &held.to_le_bytes()),
        Value::Integer(held) => fixed(out, kind::INT32, &held.to_le_bytes()),
        Value::BigInt(held) => fixed(out, kind::INT64, &held.to_le_bytes()),
        Value::HugeInt(held) => fixed(out, kind::INT128, &held.to_le_bytes()),
        Value::UTinyInt(held) => fixed(out, kind::UINT8, &held.to_le_bytes()),
        Value::USmallInt(held) => fixed(out, kind::UINT16, &held.to_le_bytes()),
        Value::UInteger(held) => fixed(out, kind::UINT32, &held.to_le_bytes()),
        Value::UBigInt(held) => fixed(out, kind::UINT64, &held.to_le_bytes()),
        Value::UHugeInt(held) => fixed(out, kind::UINT128, &held.to_le_bytes()),
        Value::Float(held) => fixed(out, kind::FLOAT, &held.to_le_bytes()),
        Value::Double(held) => fixed(out, kind::DOUBLE, &held.to_le_bytes()),
        Value::Decimal { unscaled, width, scale } => {
            out.extend([kind::DECIMAL, *width, *scale]);
            out.extend(unscaled.to_le_bytes());
        }
        Value::Varchar(text) => sized(out, kind::VARCHAR, text.as_bytes()),
        Value::Blob(held) => sized(out, kind::BLOB, held),
        Value::Bit(held) => sized(out, kind::BITSTRING, held),
        Value::BigNum(held) => sized(out, kind::BIGNUM, held),
        // The `numeric` of PostgreSQL has no kind of its own upstream, so it is kept as the text it
        // prints as.
        Value::Numeric(held) => sized(out, kind::VARCHAR, crate::numeric::to_text(held).as_bytes()),
        Value::Uuid(held) => fixed(out, kind::UUID, &held.to_le_bytes()),
        Value::Date(held) => fixed(out, kind::DATE, &held.to_le_bytes()),
        Value::Time(held) => fixed(out, kind::TIME_MICROS, &held.to_le_bytes()),
        Value::TimeTz(held) => fixed(out, kind::TIME_MICROS_TZ, &held.to_le_bytes()),
        Value::Timestamp(held) => fixed(out, kind::TIMESTAMP_MICROS, &held.to_le_bytes()),
        Value::TimestampTz(held) => fixed(out, kind::TIMESTAMP_MICROS_TZ, &held.to_le_bytes()),
        Value::TimestampS(held) => fixed(out, kind::TIMESTAMP_SEC, &held.to_le_bytes()),
        Value::TimestampMs(held) => fixed(out, kind::TIMESTAMP_MILIS, &held.to_le_bytes()),
        Value::TimestampNs(held) => fixed(out, kind::TIMESTAMP_NANOS, &held.to_le_bytes()),
        Value::Interval { months, days, micros } => {
            out.push(kind::INTERVAL);
            out.extend(months.to_le_bytes());
            out.extend(days.to_le_bytes());
            out.extend(micros.to_le_bytes());
        }
        Value::List { values, .. } => {
            out.push(kind::ARRAY);
            out.extend(count(values.len()));
            for child in values {
                write(child, out);
            }
        }
        // An unnamed struct is a row, which the pin keeps as an array. A named one is an object
        // whose keys are sorted, the last of two equal keys winning.
        Value::Struct(fields) if !fields.is_empty() && fields.iter().all(|(n, _)| n.is_empty()) => {
            out.push(kind::ARRAY);
            out.extend(count(fields.len()));
            for (_, child) in fields {
                write(child, out);
            }
        }
        Value::Struct(fields) => {
            let entries = fields
                .iter()
                .map(|(name, child)| {
                    let mut held = Vec::new();
                    write(child, &mut held);
                    (name.clone(), held)
                })
                .collect();
            out.extend(object(entries));
        }
        // A map is an array of objects with a key and a value, which is the pin's.
        Value::Map { entries, .. } => {
            out.push(kind::ARRAY);
            out.extend(count(entries.len()));
            for (key, value) in entries {
                let mut key_held = Vec::new();
                write(key, &mut key_held);
                let mut value_held = Vec::new();
                write(value, &mut value_held);
                out.extend(object(vec![
                    ("key".to_string(), key_held),
                    ("value".to_string(), value_held),
                ]));
            }
        }
        Value::Union { value, .. } => write(value, out),
        Value::Variant(held) => out.extend_from_slice(held),
    }
}

fn fixed(out: &mut Vec<u8>, kind: u8, bytes: &[u8]) {
    out.push(kind);
    out.extend_from_slice(bytes);
}

fn sized(out: &mut Vec<u8>, kind: u8, bytes: &[u8]) {
    out.push(kind);
    out.extend(count(bytes.len()));
    out.extend_from_slice(bytes);
}

fn count(len: usize) -> [u8; 4] {
    u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes()
}

/// An array of these variants.
#[must_use]
pub fn array(children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![kind::ARRAY];
    out.extend(count(children.len()));
    for child in children {
        out.extend_from_slice(child);
    }
    out
}

/// An object of these keys and variants. The keys are put in byte order, and of two that are the
/// same the later one is kept, which is what the pin does with a struct and what upstream does
/// with a JSON object since the commit the corpus is read from.
#[must_use]
pub fn object(mut entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    // A stable sort keeps equal keys in the order they came, so the last of a run is the one kept.
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut kept: Vec<(String, Vec<u8>)> = Vec::with_capacity(entries.len());
    for entry in entries {
        match kept.last_mut() {
            Some(last) if last.0 == entry.0 => *last = entry,
            _ => kept.push(entry),
        }
    }
    let mut out = vec![kind::OBJECT];
    out.extend(count(kept.len()));
    for (key, child) in kept {
        out.extend(count(key.len()));
        out.extend_from_slice(key.as_bytes());
        out.extend(child);
    }
    out
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word) as usize
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut word = [0; N];
    word.copy_from_slice(&bytes[at..at + N]);
    word
}

/// The width of the payload of a fixed width kind.
fn width(kind: u8) -> Option<usize> {
    Some(match kind {
        kind::NULL | kind::TRUE | kind::FALSE => 0,
        kind::INT8 | kind::UINT8 => 1,
        kind::INT16 | kind::UINT16 => 2,
        kind::INT32 | kind::UINT32 | kind::FLOAT | kind::DATE => 4,
        kind::INT64
        | kind::UINT64
        | kind::DOUBLE
        | kind::TIME_MICROS
        | kind::TIME_NANOS
        | kind::TIMESTAMP_SEC
        | kind::TIMESTAMP_MILIS
        | kind::TIMESTAMP_MICROS
        | kind::TIMESTAMP_NANOS
        | kind::TIME_MICROS_TZ
        | kind::TIMESTAMP_MICROS_TZ
        | kind::TIMESTAMP_NANOS_TZ => 8,
        kind::INT128 | kind::UINT128 | kind::UUID | kind::INTERVAL => 16,
        kind::DECIMAL => 18,
        _ => return None,
    })
}

/// Where the variant that starts at `at` ends.
#[must_use]
pub fn end(bytes: &[u8], at: usize) -> usize {
    let kind = bytes[at];
    if let Some(width) = width(kind) {
        return at + 1 + width;
    }
    match kind {
        kind::ARRAY => {
            let mut next = at + 5;
            for _ in 0..u32_at(bytes, at + 1) {
                next = end(bytes, next);
            }
            next
        }
        kind::OBJECT => {
            let mut next = at + 5;
            for _ in 0..u32_at(bytes, at + 1) {
                next += 4 + u32_at(bytes, next);
                next = end(bytes, next);
            }
            next
        }
        _ => at + 5 + u32_at(bytes, at + 1),
    }
}

/// The kind a variant is.
#[must_use]
pub fn kind_of(bytes: &[u8]) -> u8 {
    bytes[0]
}

/// The children of an array, in order, and empty for anything else.
#[must_use]
pub fn children(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes[0] != kind::ARRAY {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(u32_at(bytes, 1));
    let mut next = 5;
    for _ in 0..u32_at(bytes, 1) {
        let stop = end(bytes, next);
        out.push(&bytes[next..stop]);
        next = stop;
    }
    out
}

/// The keys and children of an object, in key order, and empty for anything else.
#[must_use]
pub fn entries(bytes: &[u8]) -> Vec<(&str, &[u8])> {
    if bytes[0] != kind::OBJECT {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(u32_at(bytes, 1));
    let mut next = 5;
    for _ in 0..u32_at(bytes, 1) {
        let len = u32_at(bytes, next);
        let key = std::str::from_utf8(&bytes[next + 4..next + 4 + len]).unwrap_or_default();
        next += 4 + len;
        let stop = end(bytes, next);
        out.push((key, &bytes[next..stop]));
        next = stop;
    }
    out
}

/// The child of an object under this key, which is matched exactly, case and all.
#[must_use]
pub fn field<'a>(bytes: &'a [u8], key: &str) -> Option<&'a [u8]> {
    entries(bytes).into_iter().find(|(name, _)| *name == key).map(|(_, child)| child)
}

/// The child of an array at this position, counted from zero.
#[must_use]
pub fn element(bytes: &[u8], index: usize) -> Option<&[u8]> {
    children(bytes).get(index).copied()
}

/// The variant as a value, one level deep: a scalar as the value of its type, an array as a list of
/// variants, an object as a struct of variants, and the null kind as a null.
#[must_use]
pub fn decode(bytes: &[u8]) -> Value {
    let kind = bytes[0];
    let p = &bytes[1..];
    match kind {
        kind::NULL => Value::Null,
        kind::TRUE => Value::Boolean(true),
        kind::FALSE => Value::Boolean(false),
        kind::INT8 => Value::TinyInt(i8::from_le_bytes(take(p, 0))),
        kind::INT16 => Value::SmallInt(i16::from_le_bytes(take(p, 0))),
        kind::INT32 => Value::Integer(i32::from_le_bytes(take(p, 0))),
        kind::INT64 => Value::BigInt(i64::from_le_bytes(take(p, 0))),
        kind::INT128 => Value::HugeInt(i128::from_le_bytes(take(p, 0))),
        kind::UINT8 => Value::UTinyInt(p[0]),
        kind::UINT16 => Value::USmallInt(u16::from_le_bytes(take(p, 0))),
        kind::UINT32 => Value::UInteger(u32::from_le_bytes(take(p, 0))),
        kind::UINT64 => Value::UBigInt(u64::from_le_bytes(take(p, 0))),
        kind::UINT128 => Value::UHugeInt(u128::from_le_bytes(take(p, 0))),
        kind::FLOAT => Value::Float(f32::from_le_bytes(take(p, 0))),
        kind::DOUBLE => Value::Double(f64::from_le_bytes(take(p, 0))),
        kind::DECIMAL => {
            Value::Decimal { unscaled: i128::from_le_bytes(take(p, 2)), width: p[0], scale: p[1] }
        }
        kind::VARCHAR => Value::Varchar(String::from_utf8_lossy(payload(bytes)).into_owned()),
        kind::BLOB | kind::GEOMETRY => Value::Blob(payload(bytes).to_vec()),
        kind::BITSTRING => Value::Bit(payload(bytes).to_vec()),
        kind::BIGNUM => Value::BigNum(payload(bytes).to_vec()),
        kind::UUID => Value::Uuid(i128::from_le_bytes(take(p, 0))),
        kind::DATE => Value::Date(i32::from_le_bytes(take(p, 0))),
        kind::TIME_MICROS => Value::Time(i64::from_le_bytes(take(p, 0))),
        kind::TIME_NANOS => Value::Time(i64::from_le_bytes(take(p, 0)) / 1000),
        kind::TIME_MICROS_TZ => Value::TimeTz(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_SEC => Value::TimestampS(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_MILIS => Value::TimestampMs(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_MICROS => Value::Timestamp(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_NANOS => Value::TimestampNs(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_MICROS_TZ => Value::TimestampTz(i64::from_le_bytes(take(p, 0))),
        kind::TIMESTAMP_NANOS_TZ => {
            Value::TimestampTz(i64::from_le_bytes(take(p, 0)).div_euclid(1000))
        }
        kind::INTERVAL => Value::Interval {
            months: i32::from_le_bytes(take(p, 0)),
            days: i32::from_le_bytes(take(p, 4)),
            micros: i64::from_le_bytes(take(p, 8)),
        },
        kind::ARRAY => Value::List {
            element: LogicalType::Variant,
            values: children(bytes)
                .into_iter()
                .map(|child| Value::Variant(child.to_vec()))
                .collect(),
        },
        kind::OBJECT => Value::Struct(
            entries(bytes)
                .into_iter()
                .map(|(key, child)| (key.to_string(), Value::Variant(child.to_vec())))
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// The variant as a value with every variant inside it decoded too, so an array is a list of the
/// values its children hold, which prints as the variant does.
#[must_use]
pub fn unwrapped(bytes: &[u8]) -> Value {
    match decode(bytes) {
        Value::List { element, values } => Value::List {
            element,
            values: values
                .into_iter()
                .map(|child| match child {
                    Value::Variant(held) => unwrapped(&held),
                    other => other,
                })
                .collect(),
        },
        Value::Struct(fields) => Value::Struct(
            fields
                .into_iter()
                .map(|(key, child)| match child {
                    Value::Variant(held) => (key, unwrapped(&held)),
                    other => (key, other),
                })
                .collect(),
        ),
        other => other,
    }
}

/// The bytes of a string like kind.
#[must_use]
pub fn payload(bytes: &[u8]) -> &[u8] {
    &bytes[5..5 + u32_at(bytes, 1)]
}

/// The pin's name for a kind, as its enum prints it, which is what an error about a kind says.
#[must_use]
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        kind::NULL => "VARIANT_NULL",
        kind::TRUE => "BOOL_TRUE",
        kind::FALSE => "BOOL_FALSE",
        kind::INT8 => "INT8",
        kind::INT16 => "INT16",
        kind::INT32 => "INT32",
        kind::INT64 => "INT64",
        kind::INT128 => "INT128",
        kind::UINT8 => "UINT8",
        kind::UINT16 => "UINT16",
        kind::UINT32 => "UINT32",
        kind::UINT64 => "UINT64",
        kind::UINT128 => "UINT128",
        kind::FLOAT => "FLOAT",
        kind::DOUBLE => "DOUBLE",
        kind::DECIMAL => "DECIMAL",
        kind::VARCHAR => "VARCHAR",
        kind::BLOB => "BLOB",
        kind::UUID => "UUID",
        kind::DATE => "DATE",
        kind::TIME_MICROS => "TIME_MICROS",
        kind::TIME_NANOS => "TIME_NANOS",
        kind::TIMESTAMP_SEC => "TIMESTAMP_SEC",
        kind::TIMESTAMP_MILIS => "TIMESTAMP_MILIS",
        kind::TIMESTAMP_MICROS => "TIMESTAMP_MICROS",
        kind::TIMESTAMP_NANOS => "TIMESTAMP_NANOS",
        kind::TIME_MICROS_TZ => "TIME_MICROS_TZ",
        kind::TIMESTAMP_MICROS_TZ => "TIMESTAMP_MICROS_TZ",
        kind::INTERVAL => "INTERVAL",
        kind::OBJECT => "OBJECT",
        kind::ARRAY => "ARRAY",
        kind::BIGNUM => "BIGNUM",
        kind::BITSTRING => "BITSTRING",
        kind::GEOMETRY => "GEOMETRY",
        kind::TIMESTAMP_NANOS_TZ => "TIMESTAMP_NANOS_TZ",
        _ => "INVALID",
    }
}

/// What `variant_typeof` says: the kind's name, with a decimal's width and scale, an array's
/// length and an object's keys in order spelled out after it.
#[must_use]
pub fn type_name(bytes: &[u8]) -> String {
    match bytes[0] {
        kind::DECIMAL => format!("DECIMAL({}, {})", bytes[1], bytes[2]),
        kind::ARRAY => format!("ARRAY({})", u32_at(bytes, 1)),
        kind::OBJECT => {
            let keys: Vec<&str> = entries(bytes).into_iter().map(|(key, _)| key).collect();
            format!("OBJECT({})", keys.join(", "))
        }
        other => kind_name(other).to_string(),
    }
}

/// The rank a kind sorts under, the pin's `VariantComparisonType`. Every integer, decimal and
/// bignum is one number rank and the two floats are one real rank.
fn rank(kind: u8) -> u8 {
    match kind {
        kind::TRUE | kind::FALSE => 1,
        kind::INT8..=kind::UINT128 | kind::DECIMAL | kind::BIGNUM => 2,
        kind::FLOAT | kind::DOUBLE => 3,
        kind::VARCHAR => 4,
        kind::BLOB => 5,
        kind::UUID => 6,
        kind::DATE
        | kind::TIMESTAMP_SEC
        | kind::TIMESTAMP_MILIS
        | kind::TIMESTAMP_MICROS
        | kind::TIMESTAMP_NANOS => 7,
        kind::TIMESTAMP_MICROS_TZ | kind::TIMESTAMP_NANOS_TZ => 8,
        kind::TIME_MICROS | kind::TIME_NANOS => 9,
        kind::TIME_MICROS_TZ => 10,
        kind::INTERVAL => 11,
        kind::GEOMETRY => 12,
        kind::BITSTRING => 13,
        kind::ARRAY => 14,
        kind::OBJECT => 15,
        _ => 16,
    }
}

/// Appends the bytes a variant sorts by, which are the pin's `variant_comparator`: the rank and
/// then the value written so that its bytes order as the values do. Two variants are the same value
/// when these are the same bytes, so `1`, `1::BIGINT` and `1.0` are one value and `1::DOUBLE`,
/// which is a real rather than a number, is another.
pub fn sort_key(bytes: &[u8], out: &mut Vec<u8>) {
    let kind = bytes[0];
    out.push(rank(kind));
    let p = &bytes[1..];
    match kind {
        kind::TRUE => out.push(1),
        kind::FALSE => out.push(0),
        kind::INT8 => number(
            out,
            i8::from_le_bytes(take(p, 0)) < 0,
            digits(i128::from(i8::from_le_bytes(take(p, 0))).unsigned_abs()),
            0,
        ),
        kind::INT16 => number(
            out,
            i16::from_le_bytes(take(p, 0)) < 0,
            digits(i128::from(i16::from_le_bytes(take(p, 0))).unsigned_abs()),
            0,
        ),
        kind::INT32 => number(
            out,
            i32::from_le_bytes(take(p, 0)) < 0,
            digits(i128::from(i32::from_le_bytes(take(p, 0))).unsigned_abs()),
            0,
        ),
        kind::INT64 => number(
            out,
            i64::from_le_bytes(take(p, 0)) < 0,
            digits(i128::from(i64::from_le_bytes(take(p, 0))).unsigned_abs()),
            0,
        ),
        kind::INT128 => {
            let held = i128::from_le_bytes(take(p, 0));
            number(out, held < 0, digits(held.unsigned_abs()), 0);
        }
        kind::UINT8 => number(out, false, digits(u128::from(p[0])), 0),
        kind::UINT16 => number(out, false, digits(u128::from(u16::from_le_bytes(take(p, 0)))), 0),
        kind::UINT32 => number(out, false, digits(u128::from(u32::from_le_bytes(take(p, 0)))), 0),
        kind::UINT64 => number(out, false, digits(u128::from(u64::from_le_bytes(take(p, 0)))), 0),
        kind::UINT128 => number(out, false, digits(u128::from_le_bytes(take(p, 0))), 0),
        kind::DECIMAL => {
            let held = i128::from_le_bytes(take(p, 2));
            number(out, held < 0, digits(held.unsigned_abs()), i64::from(p[1]));
        }
        kind::BIGNUM => {
            let text = crate::bignum::to_text(payload(bytes));
            let (negative, magnitude) = match text.strip_prefix('-') {
                Some(rest) => (true, rest.to_string()),
                None => (false, text),
            };
            number(out, negative, magnitude, 0);
        }
        kind::FLOAT => real(out, f64::from(f32::from_le_bytes(take(p, 0)))),
        kind::DOUBLE => real(out, f64::from_le_bytes(take(p, 0))),
        kind::UUID => signed128(out, i128::from_le_bytes(take(p, 0))),
        kind::DATE => {
            signed128(out, i128::from(i32::from_le_bytes(take(p, 0))) * 86_400_000_000_000)
        }
        kind::TIMESTAMP_SEC => {
            signed128(out, i128::from(i64::from_le_bytes(take(p, 0))) * 1_000_000_000)
        }
        kind::TIMESTAMP_MILIS => {
            signed128(out, i128::from(i64::from_le_bytes(take(p, 0))) * 1_000_000)
        }
        kind::TIMESTAMP_MICROS | kind::TIMESTAMP_MICROS_TZ => {
            signed128(out, i128::from(i64::from_le_bytes(take(p, 0))) * 1000);
        }
        kind::TIMESTAMP_NANOS | kind::TIMESTAMP_NANOS_TZ => {
            signed128(out, i128::from(i64::from_le_bytes(take(p, 0))));
        }
        kind::TIME_MICROS => signed64(out, i64::from_le_bytes(take(p, 0)).saturating_mul(1000)),
        kind::TIME_NANOS => signed64(out, i64::from_le_bytes(take(p, 0))),
        kind::TIME_MICROS_TZ => out.extend(i64::from_le_bytes(take(p, 0)).to_be_bytes()),
        kind::INTERVAL => {
            const MICROS_PER_DAY: i64 = 86_400_000_000;
            let months = i64::from(i32::from_le_bytes(take(p, 0)));
            let days = i64::from(i32::from_le_bytes(take(p, 4)));
            let micros = i64::from_le_bytes(take(p, 8));
            let days = days + micros.div_euclid(MICROS_PER_DAY);
            let micros = micros.rem_euclid(MICROS_PER_DAY);
            let months = months + days.div_euclid(30);
            let days = days.rem_euclid(30);
            let clamp =
                |n: i64| i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX });
            out.extend((clamp(months).cast_unsigned() ^ 0x8000_0000).to_be_bytes());
            out.extend((clamp(days).cast_unsigned() ^ 0x8000_0000).to_be_bytes());
            signed64(out, micros);
        }
        kind::VARCHAR => {
            out.extend(payload(bytes).iter().map(|byte| byte.wrapping_add(1)));
            out.push(0);
        }
        kind::BLOB | kind::GEOMETRY => {
            for &byte in payload(bytes) {
                if byte <= 1 {
                    out.push(1);
                }
                out.push(byte);
            }
            out.push(0);
        }
        kind::BITSTRING => {
            let bits = payload(bytes);
            for n in 0..crate::bit::len(bits) {
                out.push(if crate::bit::get(bits, n) { 3 } else { 2 });
            }
            out.push(0);
        }
        kind::ARRAY => {
            for child in children(bytes) {
                sort_key(child, out);
            }
            out.push(0);
        }
        kind::OBJECT => {
            for (key, child) in entries(bytes) {
                out.extend(key.bytes().map(|byte| byte.wrapping_add(1)));
                out.push(0);
                sort_key(child, out);
            }
            out.push(0);
        }
        _ => {}
    }
}

/// The sort key on its own.
#[must_use]
pub fn sort_key_of(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 4);
    sort_key(bytes, &mut out);
    out
}

fn digits(magnitude: u128) -> String {
    magnitude.to_string()
}

/// A number as the pin's comparator writes one: a class byte for negative, zero or positive, then
/// for a number that is not zero the adjusted exponent and the significant digits, every byte after
/// the class flipped for a negative number so that a bigger magnitude sorts first.
fn number(out: &mut Vec<u8>, negative: bool, magnitude: String, scale: i64) {
    let magnitude = magnitude.trim_start_matches('0');
    if magnitude.is_empty() {
        out.push(1);
        return;
    }
    out.push(if negative { 0 } else { 2 });
    let flip = |byte: u8| if negative { !byte } else { byte };
    #[expect(clippy::cast_possible_wrap, reason = "a number has far fewer than 2^63 digits")]
    let exponent = magnitude.len() as i64 - 1 - scale;
    for byte in (exponent.cast_unsigned() ^ (1 << 63)).to_be_bytes() {
        out.push(flip(byte));
    }
    for digit in magnitude.trim_end_matches('0').bytes() {
        out.push(flip(digit - b'0' + 1));
    }
    out.push(flip(0));
}

fn real(out: &mut Vec<u8>, x: f64) {
    let bits = if x == 0.0 {
        1 << 63
    } else if x.is_nan() {
        u64::MAX
    } else if x == f64::INFINITY {
        u64::MAX - 1
    } else if x == f64::NEG_INFINITY {
        0
    } else {
        let bits = x.to_bits();
        if bits < 1 << 63 { bits + (1 << 63) } else { !bits }
    };
    out.extend(bits.to_be_bytes());
}

fn signed64(out: &mut Vec<u8>, value: i64) {
    out.extend((value.cast_unsigned() ^ (1 << 63)).to_be_bytes());
}

fn signed128(out: &mut Vec<u8>, value: i128) {
    out.extend((value.cast_unsigned() ^ (1 << 127)).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(value: Value) -> Vec<u8> {
        encode(&value).unwrap()
    }

    #[test]
    fn a_struct_is_an_object_with_its_keys_in_byte_order() {
        let held = variant(Value::Struct(vec![
            ("b".into(), Value::Integer(1)),
            ("B".into(), Value::Varchar("x".into())),
            ("a".into(), Value::Null),
        ]));
        assert_eq!(type_name(&held), "OBJECT(B, a, b)");
        assert_eq!(kind_of(field(&held, "a").unwrap()), kind::NULL);
        assert_eq!(decode(field(&held, "b").unwrap()), Value::Integer(1));
        assert!(field(&held, "A").is_none());
    }

    #[test]
    fn numbers_of_every_width_sort_as_one_number() {
        let one = sort_key_of(&variant(Value::Integer(1)));
        assert_eq!(one, sort_key_of(&variant(Value::UBigInt(1))));
        let decimal = Value::Decimal { unscaled: 10, width: 2, scale: 1 };
        assert_eq!(one, sort_key_of(&variant(decimal)));
        assert_ne!(one, sort_key_of(&variant(Value::Double(1.0))));
        let mut keys: Vec<Vec<u8>> = [-123, 5, -1, 100, 0]
            .into_iter()
            .map(|n| sort_key_of(&variant(Value::Integer(n))))
            .collect();
        keys.sort();
        let order: Vec<Vec<u8>> = [-123, -1, 0, 5, 100]
            .into_iter()
            .map(|n| sort_key_of(&variant(Value::Integer(n))))
            .collect();
        assert_eq!(keys, order);
    }

    #[test]
    fn an_array_ends_where_its_last_child_does() {
        let held = variant(Value::List {
            element: LogicalType::Varchar,
            values: vec![Value::Varchar("ab".into()), Value::Null],
        });
        assert_eq!(end(&held, 0), held.len());
        assert_eq!(children(&held).len(), 2);
        assert_eq!(type_name(&held), "ARRAY(2)");
    }
}
