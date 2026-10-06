//! Normalized keys: the columns of a key written as bytes whose `memcmp` order is the SQL order of
//! the tuple (section 11.2).
//!
//! Each column starts with a byte that says whether it is null, `0x00` for null and `0x01` for a
//! value, so a null sorts before every value and two keys that differ only in where a null is
//! still differ. A value follows in a form that sorts as the value does:
//!
//! - a signed integer of up to 64 bits is the `i64` big-endian with the sign bit flipped, and an
//!   unsigned one the `u64` big-endian, so a `BIGINT` column probed with an `INTEGER` finds its key;
//! - a `HUGEINT`, a `UUID` and a `DECIMAL`'s unscaled integer the same in 16 bytes;
//! - a float the IEEE bits with the sign bit flipped for a positive number and every bit flipped
//!   for a negative one, with `-0.0` written as `0.0` and every NaN as one NaN that sorts last,
//!   which is how the pin compares them;
//! - text and bytes as they are, with a `0x00` byte written as `0x00 0xFF` and a `0x00` after the
//!   last byte, so that a string sorts before every string it is a prefix of.
//!
//! A column of any other type, such as a list or an interval, is not written: [`push`] answers
//! `false`, and the caller keeps such a key some other way.

use rudb_common::Value;

/// The byte a null column starts with.
const NULL: u8 = 0x00;
/// The byte a column holding a value starts with.
const VALUE: u8 = 0x01;
/// The byte that follows a `0x00` inside text to say it is a byte of the text.
const ESCAPED: u8 = 0xFF;

/// Writes the normalized form of a key of `values` to `out`, after what is there, and answers
/// whether every value is of a type a key can be written for. On `false` `out` holds part of a
/// key and the caller should not use it.
pub fn normalize(values: &[Value], out: &mut Vec<u8>) -> bool {
    values.iter().all(|value| push(value, out))
}

/// Writes one column of a key to `out`, after what is there. See [`normalize`].
pub fn push(value: &Value, out: &mut Vec<u8>) -> bool {
    let signed =
        |out: &mut Vec<u8>, v: i64| out.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
    let wide = |out: &mut Vec<u8>, v: i128| {
        out.extend_from_slice(&((v as u128) ^ (1 << 127)).to_be_bytes())
    };
    if matches!(value, Value::Null) {
        out.push(NULL);
        return true;
    }
    let at = out.len();
    out.push(VALUE);
    match value {
        Value::Boolean(v) => out.push(u8::from(*v)),
        Value::TinyInt(v) => signed(out, i64::from(*v)),
        Value::SmallInt(v) => signed(out, i64::from(*v)),
        Value::Integer(v) | Value::Date(v) => signed(out, i64::from(*v)),
        Value::BigInt(v)
        | Value::Time(v)
        | Value::TimeTz(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v)
        | Value::TimestampS(v)
        | Value::TimestampMs(v)
        | Value::TimestampNs(v) => signed(out, *v),
        Value::UTinyInt(v) => out.extend_from_slice(&u64::from(*v).to_be_bytes()),
        Value::USmallInt(v) => out.extend_from_slice(&u64::from(*v).to_be_bytes()),
        Value::UInteger(v) => out.extend_from_slice(&u64::from(*v).to_be_bytes()),
        Value::UBigInt(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::HugeInt(v) | Value::Uuid(v) => wide(out, *v),
        Value::Decimal { unscaled, .. } => wide(out, *unscaled),
        Value::UHugeInt(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Float(v) => out.extend_from_slice(&float(f64::from(*v)).to_be_bytes()),
        Value::Double(v) => out.extend_from_slice(&float(*v).to_be_bytes()),
        Value::Varchar(v) => text(v.as_bytes(), out),
        Value::Blob(v) | Value::Bit(v) | Value::BigNum(v) => text(v, out),
        _ => {
            out.truncate(at);
            return false;
        }
    }
    true
}

/// The bits of `v` turned so that they sort as the number does.
///
/// A `FLOAT` is widened to a `DOUBLE` first, which is exact, so the two write the same key for the
/// same number.
fn float(v: f64) -> u64 {
    let v = if v.is_nan() {
        f64::NAN
    } else if v == 0.0 {
        0.0
    } else {
        v
    };
    let bits = v.to_bits();
    // `f64::NAN` has the sign bit clear, so it lands above positive infinity.
    if bits >> 63 == 0 { bits | (1 << 63) } else { !bits }
}

/// Writes `bytes` with every `0x00` escaped and a `0x00` after them.
fn text(bytes: &[u8], out: &mut Vec<u8>) {
    let mut rest = bytes;
    while let Some(zero) = rest.iter().position(|&b| b == 0) {
        out.extend_from_slice(&rest[..=zero]);
        out.push(ESCAPED);
        rest = &rest[zero + 1..];
    }
    out.extend_from_slice(rest);
    out.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(values: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(normalize(values, &mut out), "{values:?}");
        out
    }

    /// Checks that the keys of `values` sort in the order they are given in.
    fn ascending(values: &[Value]) {
        let keys: Vec<_> = values.iter().map(|v| key(std::slice::from_ref(v))).collect();
        for (pair, values) in keys.windows(2).zip(values.windows(2)) {
            assert!(pair[0] < pair[1], "{:?} should sort before {:?}", values[0], values[1]);
        }
    }

    #[test]
    fn integers_sort_as_numbers() {
        ascending(&[
            Value::Null,
            Value::BigInt(i64::MIN),
            Value::BigInt(-300),
            Value::Integer(-1),
            Value::TinyInt(0),
            Value::SmallInt(1),
            Value::BigInt(256),
            Value::BigInt(i64::MAX),
        ]);
        ascending(&[Value::UTinyInt(0), Value::UInteger(255), Value::UBigInt(u64::MAX)]);
        ascending(&[
            Value::HugeInt(i128::MIN),
            Value::HugeInt(-1),
            Value::HugeInt(0),
            Value::HugeInt(i128::MAX),
        ]);
        ascending(&[
            Value::Decimal { unscaled: -1234, width: 9, scale: 2 },
            Value::Decimal { unscaled: 5, width: 9, scale: 2 },
        ]);
        assert_eq!(key(&[Value::Integer(7)]), key(&[Value::BigInt(7)]));
    }

    #[test]
    fn floats_sort_as_the_pin_compares_them() {
        ascending(&[
            Value::Double(f64::NEG_INFINITY),
            Value::Double(-2.5),
            Value::Double(-f64::MIN_POSITIVE),
            Value::Double(0.0),
            Value::Double(f64::MIN_POSITIVE),
            Value::Float(1.5),
            Value::Double(f64::INFINITY),
            Value::Double(f64::NAN),
        ]);
        assert_eq!(key(&[Value::Double(-0.0)]), key(&[Value::Double(0.0)]));
        assert_eq!(key(&[Value::Double(-f64::NAN)]), key(&[Value::Double(f64::NAN)]));
        assert_eq!(key(&[Value::Float(1.5)]), key(&[Value::Double(1.5)]));
    }

    #[test]
    fn text_sorts_before_what_it_is_a_prefix_of() {
        let text = |s: &str| Value::Varchar(s.to_owned());
        ascending(&[
            text(""),
            text("a"),
            text("a\0"),
            text("a\0b"),
            text("a\x01"),
            text("ab"),
            text("b"),
        ]);
        ascending(&[
            Value::Blob(vec![]),
            Value::Blob(vec![0]),
            Value::Blob(vec![0, 0]),
            Value::Blob(vec![0xFF]),
        ]);
    }

    #[test]
    fn a_key_of_several_columns_sorts_by_the_first_then_the_next() {
        let text = |s: &str| Value::Varchar(s.to_owned());
        let rows = [
            vec![text("a"), Value::Null],
            vec![text("a"), Value::Integer(-5)],
            vec![text("a"), Value::Integer(3)],
            vec![text("a\0"), Value::Integer(0)],
            vec![text("ab"), Value::Null],
            vec![Value::Null, Value::Integer(0)],
        ];
        let mut keys: Vec<_> = rows.iter().map(|row| key(row)).collect();
        let null = keys.pop().expect("a row");
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(null < keys[0]);
    }

    #[test]
    fn a_column_of_another_type_is_refused_and_leaves_nothing() {
        let mut out = vec![9];
        assert!(!push(&Value::Interval { months: 1, days: 0, micros: 0 }, &mut out));
        assert_eq!(out, [9]);
    }
}
