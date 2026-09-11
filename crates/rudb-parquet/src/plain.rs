//! Plain encoding: the values as themselves, which is what every other encoding falls back to.
//!
//! Fixed width types are little endian and packed with no padding, so a page of `INT32` is four
//! bytes a value and nothing else. Booleans are the exception and are bit packed one bit a value,
//! least significant bit first, the same bit order as [`crate::hybrid`]. Byte arrays carry a four
//! byte little endian length in front of each value, and fixed length byte arrays carry no length
//! because the schema stated it.
//!
//! `INT96` is decoded here and nowhere else. It is a twelve byte timestamp that the format
//! deprecated in 2017 and that parquet-java wrote by default for years before that, so files full of
//! it will be read long after nobody writes it. The layout is nanoseconds within the day in the
//! first eight bytes and a Julian day number in the last four, and it comes out of here as
//! nanoseconds since the Unix epoch, which is what the schema said the column was.
//!
//! Nothing here copies a byte array. The values borrow the page they were decoded from, which is
//! what lets a dictionary page be decoded once and pointed at by every data page in the chunk.

use rudb_common::{Error, Result};

use crate::metadata::Physical;

/// The values of one page, in whichever form the physical type gives.
///
/// Six variants for seven physical types, because `INT96` arrives as `Int64` already converted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Values<'a> {
    /// `BOOLEAN`.
    Bool(Vec<bool>),
    /// `INT32`.
    Int32(Vec<i32>),
    /// `INT64`, and `INT96` after conversion.
    Int64(Vec<i64>),
    /// `FLOAT`.
    Float(Vec<f32>),
    /// `DOUBLE`.
    Double(Vec<f64>),
    /// `BYTE_ARRAY` and `FIXED_LEN_BYTE_ARRAY`, borrowed from the page.
    Bytes(Vec<&'a [u8]>),
}

impl Values<'_> {
    /// How many values there are.
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Bool(values) => values.len(),
            Self::Int32(values) => values.len(),
            Self::Int64(values) => values.len(),
            Self::Float(values) => values.len(),
            Self::Double(values) => values.len(),
            Self::Bytes(values) => values.len(),
        }
    }
}

/// Days between the Julian epoch and the Unix epoch, which is what an `INT96` timestamp counts from.
const JULIAN_UNIX_EPOCH: i64 = 2_440_588;

/// Nanoseconds in a day.
const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// Decodes `count` plainly encoded values of `physical` out of `bytes`.
///
/// `width` is the schema's declared length and is used only by `FIXED_LEN_BYTE_ARRAY`.
///
/// # Errors
///
/// If the page ends before `count` values have been read, or a byte array states a length that runs
/// past the end of the page, or a fixed length column declares a width that is not one.
pub(crate) fn decode(
    physical: Physical,
    width: i32,
    bytes: &[u8],
    count: usize,
) -> Result<Values<'_>> {
    Ok(match physical {
        Physical::Boolean => Values::Bool(booleans(bytes, count)?),
        Physical::Int32 => Values::Int32(fixed(bytes, count, 4, |run| {
            i32::from_le_bytes([run[0], run[1], run[2], run[3]])
        })?),
        Physical::Int64 => Values::Int64(fixed(bytes, count, 8, le_i64)?),
        Physical::Int96 => Values::Int64(fixed(bytes, count, 12, |run| {
            let nanos = le_i64(&run[..8]);
            let day = i64::from(i32::from_le_bytes([run[8], run[9], run[10], run[11]]));
            (day - JULIAN_UNIX_EPOCH).saturating_mul(NANOS_PER_DAY).saturating_add(nanos)
        })?),
        Physical::Float => Values::Float(fixed(bytes, count, 4, |run| {
            f32::from_le_bytes([run[0], run[1], run[2], run[3]])
        })?),
        Physical::Double => Values::Double(fixed(bytes, count, 8, |run| {
            f64::from_le_bytes([run[0], run[1], run[2], run[3], run[4], run[5], run[6], run[7]])
        })?),
        Physical::ByteArray => Values::Bytes(byte_arrays(bytes, count)?),
        Physical::FixedLenByteArray => {
            let width = usize::try_from(width).map_err(|_| {
                Error::io(format!("a fixed length column of width {width}, which is not a width"))
            })?;
            Values::Bytes(fixed(bytes, count, width, |run| run)?)
        }
    })
}

/// Eight bytes as a little endian signed integer.
fn le_i64(run: &[u8]) -> i64 {
    i64::from_le_bytes([run[0], run[1], run[2], run[3], run[4], run[5], run[6], run[7]])
}

/// Reads `count` values of `size` bytes each, handing each one's bytes to `read`.
///
/// A size of zero is legal, because `FIXED_LEN_BYTE_ARRAY(0)` is a column of empty values, and it is
/// handled by the length check below rather than by dividing.
fn fixed<'a, T>(
    bytes: &'a [u8],
    count: usize,
    size: usize,
    read: impl Fn(&'a [u8]) -> T,
) -> Result<Vec<T>> {
    let needed = count.checked_mul(size).ok_or_else(|| Error::io("a parquet page that wraps"))?;
    if bytes.len() < needed {
        return Err(Error::io(format!(
            "a parquet page of {} bytes holding {count} values of {size} bytes",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(count);
    for at in 0..count {
        out.push(read(&bytes[at * size..at * size + size]));
    }
    Ok(out)
}

/// Reads `count` bit packed booleans, least significant bit of the first byte first.
fn booleans(bytes: &[u8], count: usize) -> Result<Vec<bool>> {
    if bytes.len() * 8 < count {
        return Err(Error::io(format!(
            "a parquet page of {} bytes holding {count} booleans",
            bytes.len()
        )));
    }
    let mut out = Vec::with_capacity(count);
    for at in 0..count {
        out.push(bytes[at / 8] >> (at % 8) & 1 == 1);
    }
    Ok(out)
}

/// Reads `count` length prefixed byte arrays, each borrowed from the page.
fn byte_arrays(bytes: &[u8], count: usize) -> Result<Vec<&[u8]>> {
    let mut out = Vec::with_capacity(count);
    let mut at = 0;
    for _ in 0..count {
        let header = bytes
            .get(at..at + 4)
            .ok_or_else(|| Error::io("a parquet page that ends inside a byte array length"))?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        at += 4;
        let end = at
            .checked_add(len)
            .ok_or_else(|| Error::io("a parquet byte array length that wraps"))?;
        let value = bytes.get(at..end).ok_or_else(|| {
            Error::io(format!("a parquet byte array of {len} bytes past the end of its page"))
        })?;
        out.push(value);
        at = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Values, decode};
    use crate::metadata::Physical;

    #[test]
    fn fixed_width_values_are_little_endian_and_packed() {
        let bytes = [1_u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(decode(Physical::Int32, 0, &bytes, 2).unwrap(), Values::Int32(vec![1, -1]));
    }

    #[test]
    fn a_double_reads_back_as_the_double_that_was_written() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.5_f64.to_le_bytes());
        bytes.extend_from_slice(&(-3.0_f64).to_le_bytes());
        assert_eq!(
            decode(Physical::Double, 0, &bytes, 2).unwrap(),
            Values::Double(vec![1.5, -3.0])
        );
    }

    #[test]
    fn booleans_are_one_bit_each_with_the_first_value_in_the_low_bit() {
        assert_eq!(
            decode(Physical::Boolean, 0, &[0b0000_0101], 4).unwrap(),
            Values::Bool(vec![true, false, true, false])
        );
    }

    #[test]
    fn a_byte_array_carries_its_own_length_and_borrows_its_bytes() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(b"one");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&5_u32.to_le_bytes());
        bytes.extend_from_slice(b"three");
        let values = decode(Physical::ByteArray, 0, &bytes, 3).unwrap();
        assert_eq!(values, Values::Bytes(vec![b"one".as_slice(), b"", b"three".as_slice()]));
    }

    #[test]
    fn a_fixed_length_byte_array_takes_its_width_from_the_schema() {
        let values = decode(Physical::FixedLenByteArray, 2, b"abcdef", 3).unwrap();
        assert_eq!(values, Values::Bytes(vec![b"ab".as_slice(), b"cd", b"ef"]));
    }

    #[test]
    fn an_int96_comes_out_as_nanoseconds_since_the_unix_epoch() {
        // The Julian day of the Unix epoch with no nanoseconds into it is zero.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.extend_from_slice(&2_440_588_i32.to_le_bytes());
        // One day later, one second into it.
        bytes.extend_from_slice(&1_000_000_000_i64.to_le_bytes());
        bytes.extend_from_slice(&2_440_589_i32.to_le_bytes());
        assert_eq!(
            decode(Physical::Int96, 0, &bytes, 2).unwrap(),
            Values::Int64(vec![0, 86_401_000_000_000])
        );
    }

    #[test]
    fn a_page_shorter_than_the_values_it_claims_is_an_error() {
        let error = decode(Physical::Int32, 0, &[0, 0, 0], 1).unwrap_err();
        assert!(error.to_string().contains("holding 1 values of 4 bytes"), "{error}");
    }

    #[test]
    fn a_byte_array_that_runs_past_its_page_is_an_error_rather_than_a_short_value() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&9_u32.to_le_bytes());
        bytes.extend_from_slice(b"four");
        let error = decode(Physical::ByteArray, 0, &bytes, 1).unwrap_err();
        assert!(error.to_string().contains("past the end of its page"), "{error}");
    }
}
