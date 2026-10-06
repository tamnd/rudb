//! A nested column on disk, as one byte string a row.
//!
//! A nested page is a blob page, and a row is its value in the bytes of its type. A list or an
//! array is the count of its elements and then each element, a byte that says whether it is null
//! and then its bytes. A struct is each field in the order of its type, a null byte and then the
//! bytes, the same way. A map is the count of its entries and then each key and each value. A
//! union is the position of the member it holds and then that member. The types are in the
//! directory, after the tag of the column, so a row holds no type and no width.
//!
//! This is the plain first version. It keeps a nested column through a checkpoint, and it does not
//! try to be fast: a scan of a nested column builds one `Value` a row. A nested column with its
//! children in pages of their own is the version to come when a query needs it.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::Vector;

use super::invalid;

/// Whether a column of `ty` is written by this module rather than as a page of its own type.
pub(super) fn holds(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::List(_)
            | LogicalType::Array(..)
            | LogicalType::Struct(_)
            | LogicalType::Map(..)
            | LogicalType::Union(_)
    )
}

/// The column of byte strings that holds a nested column on disk.
pub(super) fn to_bytes(vector: &Vector) -> Result<Vector> {
    let ty = vector.logical_type();
    if !holds(ty) {
        return Err(Error::internal("a nested page was asked of a column that is not nested"));
    }
    let mut rows = Vec::with_capacity(vector.len());
    // row at a time: a nested page is written from the value of each row, which has no flat layout.
    for row in 0..vector.len() {
        rows.push(match vector.try_value_at(row)? {
            Value::Null => Value::Null,
            value => {
                let mut out = Vec::new();
                put(&mut out, &value, ty)?;
                Value::Blob(out)
            }
        });
    }
    Vector::from_values(LogicalType::Blob, &rows)
}

/// The nested column of `ty` that a column of byte strings from [`to_bytes`] holds.
pub(super) fn from_bytes(ty: &LogicalType, bytes: &Vector) -> Result<Vector> {
    if !holds(ty) {
        return Err(Error::internal("a nested page was read as a column that is not nested"));
    }
    let mut rows = Vec::with_capacity(bytes.len());
    // row at a time: a nested page is read back into the value of each row, which has no flat
    // layout.
    for row in 0..bytes.len() {
        rows.push(match bytes.try_value_at(row)? {
            Value::Null => Value::Null,
            Value::Blob(held) => {
                let mut cur = Reader { bytes: &held, at: 0 };
                let value = get(&mut cur, ty)?;
                if cur.at != held.len() {
                    return Err(invalid("nested value has trailing bytes"));
                }
                value
            }
            _ => return Err(invalid("nested page holds a value that is not bytes")),
        });
    }
    Vector::from_values(ty.clone(), &rows)
}

fn put_list(out: &mut Vec<u8>, values: &[Value], element: &LogicalType) -> Result<()> {
    put_len(out, values.len())?;
    for value in values {
        put_maybe(out, value, element)?;
    }
    Ok(())
}

/// A value that may be null, as the byte that says which and then the value when there is one.
fn put_maybe(out: &mut Vec<u8>, value: &Value, ty: &LogicalType) -> Result<()> {
    if value.is_null() {
        out.push(0);
    } else {
        out.push(1);
        put(out, value, ty)?;
    }
    Ok(())
}

fn put_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len = u32::try_from(len)
        .map_err(|_| Error::invalid_input(format!("a list value of {len} items is too long")))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    put_len(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// One element that is not null, in the bytes of its type.
fn put(out: &mut Vec<u8>, value: &Value, ty: &LogicalType) -> Result<()> {
    match (ty, value) {
        (LogicalType::Boolean, Value::Boolean(held)) => out.push(u8::from(*held)),
        (LogicalType::TinyInt, Value::TinyInt(held)) => out.extend_from_slice(&held.to_le_bytes()),
        (LogicalType::UTinyInt, Value::UTinyInt(held)) => out.push(*held),
        (LogicalType::SmallInt, Value::SmallInt(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        (LogicalType::USmallInt, Value::USmallInt(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        (LogicalType::Integer, Value::Integer(held)) | (LogicalType::Date, Value::Date(held)) => {
            out.extend_from_slice(&held.to_le_bytes())
        }
        (LogicalType::UInteger, Value::UInteger(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        (LogicalType::Float, Value::Float(held)) => out.extend_from_slice(&held.to_le_bytes()),
        (LogicalType::BigInt, Value::BigInt(held))
        | (LogicalType::Time, Value::Time(held))
        | (LogicalType::TimeTz, Value::TimeTz(held))
        | (LogicalType::Timestamp, Value::Timestamp(held))
        | (LogicalType::TimestampTz, Value::TimestampTz(held))
        | (LogicalType::TimestampS, Value::TimestampS(held))
        | (LogicalType::TimestampMs, Value::TimestampMs(held))
        | (LogicalType::TimestampNs, Value::TimestampNs(held))
        | (LogicalType::TimeNs, Value::TimeNs(held))
        | (LogicalType::TimestampTzNs, Value::TimestampTzNs(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        (LogicalType::UBigInt, Value::UBigInt(held)) => out.extend_from_slice(&held.to_le_bytes()),
        (LogicalType::Double, Value::Double(held)) => out.extend_from_slice(&held.to_le_bytes()),
        (LogicalType::HugeInt, Value::HugeInt(held)) | (LogicalType::Uuid, Value::Uuid(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        (LogicalType::UHugeInt, Value::UHugeInt(held)) => {
            out.extend_from_slice(&held.to_le_bytes());
        }
        // The unscaled integer only. The width and the scale are the column's, in the directory.
        (LogicalType::Decimal { scale, .. }, Value::Decimal { unscaled, scale: held, .. })
            if scale == held =>
        {
            out.extend_from_slice(&unscaled.to_le_bytes());
        }
        (LogicalType::Interval, Value::Interval { months, days, micros }) => {
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&micros.to_le_bytes());
        }
        // An enum element is written as its label, which the column's type turns back into its
        // position when the value is read. A JSON or JSONB document is held as its text.
        (
            LogicalType::Varchar | LogicalType::Enum(_) | LogicalType::Json | LogicalType::Jsonb,
            Value::Varchar(held),
        ) => {
            put_bytes(out, held.as_bytes())?;
        }
        (LogicalType::Blob, Value::Blob(held))
        | (LogicalType::Bit, Value::Bit(held))
        | (LogicalType::BigNum, Value::BigNum(held))
        | (LogicalType::Numeric, Value::Numeric(held))
        | (LogicalType::Variant, Value::Variant(held)) => put_bytes(out, held)?,
        (
            LogicalType::List(element) | LogicalType::Array(element, _),
            Value::List { values, .. },
        ) => {
            put_list(out, values, element)?;
        }
        (LogicalType::Struct(fields), Value::Struct(held)) if held.len() == fields.len() => {
            for (field, (_, value)) in fields.iter().zip(held) {
                put_maybe(out, value, &field.ty)?;
            }
        }
        (LogicalType::Map(key, held), Value::Map { entries, .. }) => {
            put_len(out, entries.len())?;
            for (one, other) in entries {
                put_maybe(out, one, key)?;
                put_maybe(out, other, held)?;
            }
        }
        (LogicalType::Union(members), Value::Union { tag, value, .. }) => {
            let member = members.get(usize::from(*tag)).ok_or_else(|| mismatch(value, ty))?;
            out.push(*tag);
            put_maybe(out, value, &member.ty)?;
        }
        _ => return Err(mismatch(value, ty)),
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).filter(|&end| end <= self.bytes.len());
        let Some(end) = end else {
            return Err(invalid("nested value ends before its elements do"));
        };
        let held = &self.bytes[self.at..end];
        self.at = end;
        Ok(held)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("the length asked for"))
    }

    fn len(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.array()?) as usize)
    }
}

fn get_list(cur: &mut Reader<'_>, element: &LogicalType) -> Result<Vec<Value>> {
    let count = cur.len()?;
    // Each element is one byte at least, so a count larger than the bytes left is damage and
    // not a reason to reserve that much memory.
    if count > cur.bytes.len() - cur.at {
        return Err(invalid("list value counts more elements than it holds"));
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(get_maybe(cur, element)?);
    }
    Ok(values)
}

/// The other half of [`put_maybe`].
fn get_maybe(cur: &mut Reader<'_>, ty: &LogicalType) -> Result<Value> {
    match cur.take(1)?[0] {
        0 => Ok(Value::Null),
        1 => get(cur, ty),
        _ => Err(invalid("nested value null flag differs")),
    }
}

fn get(cur: &mut Reader<'_>, ty: &LogicalType) -> Result<Value> {
    Ok(match ty {
        LogicalType::Boolean => Value::Boolean(cur.take(1)?[0] != 0),
        LogicalType::TinyInt => Value::TinyInt(i8::from_le_bytes(cur.array()?)),
        LogicalType::UTinyInt => Value::UTinyInt(cur.take(1)?[0]),
        LogicalType::SmallInt => Value::SmallInt(i16::from_le_bytes(cur.array()?)),
        LogicalType::USmallInt => Value::USmallInt(u16::from_le_bytes(cur.array()?)),
        LogicalType::Integer => Value::Integer(i32::from_le_bytes(cur.array()?)),
        LogicalType::Date => Value::Date(i32::from_le_bytes(cur.array()?)),
        LogicalType::UInteger => Value::UInteger(u32::from_le_bytes(cur.array()?)),
        LogicalType::Float => Value::Float(f32::from_le_bytes(cur.array()?)),
        LogicalType::BigInt => Value::BigInt(i64::from_le_bytes(cur.array()?)),
        LogicalType::Time => Value::Time(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimeTz => Value::TimeTz(i64::from_le_bytes(cur.array()?)),
        LogicalType::Timestamp => Value::Timestamp(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimestampTz => Value::TimestampTz(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimestampS => Value::TimestampS(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimestampMs => Value::TimestampMs(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimestampNs => Value::TimestampNs(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimeNs => Value::TimeNs(i64::from_le_bytes(cur.array()?)),
        LogicalType::TimestampTzNs => Value::TimestampTzNs(i64::from_le_bytes(cur.array()?)),
        LogicalType::UBigInt => Value::UBigInt(u64::from_le_bytes(cur.array()?)),
        LogicalType::Double => Value::Double(f64::from_le_bytes(cur.array()?)),
        LogicalType::HugeInt => Value::HugeInt(i128::from_le_bytes(cur.array()?)),
        LogicalType::Uuid => Value::Uuid(i128::from_le_bytes(cur.array()?)),
        LogicalType::UHugeInt => Value::UHugeInt(u128::from_le_bytes(cur.array()?)),
        LogicalType::Decimal { width, scale } => Value::Decimal {
            unscaled: i128::from_le_bytes(cur.array()?),
            width: *width,
            scale: *scale,
        },
        LogicalType::Interval => Value::Interval {
            months: i32::from_le_bytes(cur.array()?),
            days: i32::from_le_bytes(cur.array()?),
            micros: i64::from_le_bytes(cur.array()?),
        },
        LogicalType::Varchar | LogicalType::Enum(_) | LogicalType::Json | LogicalType::Jsonb => {
            let len = cur.len()?;
            let text = std::str::from_utf8(cur.take(len)?)
                .map_err(|_| invalid("list element of a varchar list is not UTF-8"))?;
            Value::Varchar(text.to_owned())
        }
        LogicalType::Blob
        | LogicalType::Bit
        | LogicalType::BigNum
        | LogicalType::Numeric
        | LogicalType::Variant => {
            let len = cur.len()?;
            let held = cur.take(len)?.to_vec();
            match ty {
                LogicalType::Blob => Value::Blob(held),
                LogicalType::Bit => Value::Bit(held),
                LogicalType::Numeric => Value::Numeric(held),
                LogicalType::Variant => Value::Variant(held),
                _ => Value::BigNum(held),
            }
        }
        LogicalType::List(element) => {
            Value::List { element: (**element).clone(), values: get_list(cur, element)? }
        }
        LogicalType::Array(element, len) => {
            let values = get_list(cur, element)?;
            if values.len() != *len as usize {
                return Err(invalid("array value holds the wrong number of elements"));
            }
            Value::List { element: (**element).clone(), values }
        }
        LogicalType::Struct(fields) => Value::Struct(
            fields
                .iter()
                .map(|field| Ok((field.name.clone(), get_maybe(cur, &field.ty)?)))
                .collect::<Result<_>>()?,
        ),
        LogicalType::Map(key, held) => {
            let count = cur.len()?;
            // Each entry is two bytes at least, for the same reason as in `get_list`.
            if count > (cur.bytes.len() - cur.at) / 2 {
                return Err(invalid("map value counts more entries than it holds"));
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push((get_maybe(cur, key)?, get_maybe(cur, held)?));
            }
            Value::map((**key).clone(), (**held).clone(), entries)
        }
        LogicalType::Union(members) => {
            let tag = cur.take(1)?[0];
            let member = members
                .get(usize::from(tag))
                .ok_or_else(|| invalid("union value holds a member its type does not have"))?;
            let value = get_maybe(cur, &member.ty)?;
            Value::Union { members: members.clone(), tag, value: Box::new(value) }
        }
        _ => return Err(Error::not_implemented(format!("native storage for a nested {ty}"))),
    })
}

fn mismatch(value: &Value, ty: &LogicalType) -> Error {
    Error::internal(format!("a nested column of {ty} holds the value {value:?}"))
}
