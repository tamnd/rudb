//! A list column on disk, as one byte string a row.
//!
//! A list page is a blob page. Each row is the count of its elements and then each element, a
//! byte that says whether it is null and then its bytes. The element type is in the directory,
//! after the tag of the list, so a row holds no type and no width.
//!
//! This is the plain first version. It keeps a list column through a checkpoint, and it does not
//! try to be fast: a scan of a list column builds one `Value` an element. A list column with its
//! elements in a child page of their own is the version to come when a query needs it.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::Vector;

use super::invalid;

/// The column of byte strings that holds a list column on disk.
pub(super) fn to_bytes(vector: &Vector) -> Result<Vector> {
    let LogicalType::List(element) = vector.logical_type() else {
        return Err(Error::internal("a list page was asked of a column that is not a list"));
    };
    let mut rows = Vec::with_capacity(vector.len());
    for row in 0..vector.len() {
        rows.push(match vector.try_value_at(row)? {
            Value::Null => Value::Null,
            Value::List { values, .. } => {
                let mut out = Vec::new();
                put_list(&mut out, &values, element)?;
                Value::Blob(out)
            }
            other => return Err(mismatch(&other, vector.logical_type())),
        });
    }
    Vector::from_values(LogicalType::Blob, &rows)
}

/// The list column of `ty` that a column of byte strings from [`to_bytes`] holds.
pub(super) fn from_bytes(ty: &LogicalType, bytes: &Vector) -> Result<Vector> {
    let LogicalType::List(element) = ty else {
        return Err(Error::internal("a list page was read as a column that is not a list"));
    };
    let mut rows = Vec::with_capacity(bytes.len());
    for row in 0..bytes.len() {
        rows.push(match bytes.try_value_at(row)? {
            Value::Null => Value::Null,
            Value::Blob(held) => {
                let mut cur = Reader { bytes: &held, at: 0 };
                let values = get_list(&mut cur, element)?;
                if cur.at != held.len() {
                    return Err(invalid("list value has trailing bytes"));
                }
                Value::List { element: (**element).clone(), values }
            }
            _ => return Err(invalid("list page holds a value that is not bytes")),
        });
    }
    Vector::from_values(ty.clone(), &rows)
}

fn put_list(out: &mut Vec<u8>, values: &[Value], element: &LogicalType) -> Result<()> {
    put_len(out, values.len())?;
    for value in values {
        if value.is_null() {
            out.push(0);
        } else {
            out.push(1);
            put(out, value, element)?;
        }
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
        | (LogicalType::TimestampNs, Value::TimestampNs(held)) => {
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
        (LogicalType::Varchar, Value::Varchar(held)) => put_bytes(out, held.as_bytes())?,
        (LogicalType::Blob, Value::Blob(held))
        | (LogicalType::Bit, Value::Bit(held))
        | (LogicalType::BigNum, Value::BigNum(held)) => put_bytes(out, held)?,
        (LogicalType::List(element), Value::List { values, .. }) => {
            put_list(out, values, element)?;
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
            return Err(invalid("list value ends before its elements do"));
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
        values.push(match cur.take(1)?[0] {
            0 => Value::Null,
            1 => get(cur, element)?,
            _ => return Err(invalid("list element null flag differs")),
        });
    }
    Ok(values)
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
        LogicalType::Varchar => {
            let len = cur.len()?;
            let text = std::str::from_utf8(cur.take(len)?)
                .map_err(|_| invalid("list element of a varchar list is not UTF-8"))?;
            Value::Varchar(text.to_owned())
        }
        LogicalType::Blob | LogicalType::Bit | LogicalType::BigNum => {
            let len = cur.len()?;
            let held = cur.take(len)?.to_vec();
            match ty {
                LogicalType::Blob => Value::Blob(held),
                LogicalType::Bit => Value::Bit(held),
                _ => Value::BigNum(held),
            }
        }
        LogicalType::List(element) => {
            Value::List { element: (**element).clone(), values: get_list(cur, element)? }
        }
        _ => return Err(Error::not_implemented(format!("native storage for a list of {ty}"))),
    })
}

fn mismatch(value: &Value, ty: &LogicalType) -> Error {
    Error::internal(format!("a list column of {ty} holds the value {value:?}"))
}
