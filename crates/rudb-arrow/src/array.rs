//! One column, in Arrow's memory layout.
//!
//! Arrow says an array is a length, a null count, and a list of buffers whose meaning comes from
//! the type: a validity bitmap first, then offsets for a variable width type, then the values. That
//! is what this builds, as owned little endian bytes, which is the form the C data interface hands
//! across a boundary and the form a reader on the other side of one already knows how to read.
//!
//! Nothing here is zero copy yet and the crate's own description promises it is where the layouts
//! permit. Two of the three buffers already do permit it. Our validity bitmap is the same LSB first
//! layout as Arrow's, and a run of `i32` is a run of `i32`, so the copy is a memcpy that a later
//! change can drop once there is an owner to hand the pages to. The one that cannot is `VARCHAR`:
//! we store a sixteen byte view and an arena and Arrow's `u` is offsets and a contiguous run of
//! bytes, and no arrangement of the two is the other one.

use rudb_common::{Error, LogicalType, Result};
use rudb_vector::{Data, Validity, Vector};

use crate::types::DataType;

/// A column of values, in Arrow's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Array {
    data_type: DataType,
    len: usize,
    null_count: usize,
    validity: Option<Vec<u8>>,
    offsets: Option<Vec<u8>>,
    values: Vec<u8>,
}

impl Array {
    /// The Arrow array a vector becomes.
    ///
    /// The vector is flattened first, so a constant, a sequence and a dictionary all arrive here as
    /// the values they stand for. Arrow has a run end encoding and a dictionary array of its own
    /// and they are worth having later, but an export that sometimes hands back a dictionary is an
    /// export every reader has to have two paths for, and the reader is the one we are trying to
    /// make cheap.
    ///
    /// # Errors
    ///
    /// For a type with no Arrow counterpart, and for a vector whose values are not the layout its
    /// type says they are.
    pub fn of(vector: &Vector) -> Result<Self> {
        let flat = vector.flatten()?;
        let data_type = DataType::of(flat.logical_type())?;
        let len = flat.len();
        let null_count = len - flat.validity().count_valid(len);
        let validity = bitmap(flat.validity(), len);
        let empty = Data::Empty;
        let data = flat.data().unwrap_or(&empty);
        let (values, offsets) = match &data_type {
            // The null type has no buffers at all, so there is nothing to read and nothing to
            // write. Everything in it is null by being that type.
            DataType::Null => (Vec::new(), None),
            DataType::Boolean => (bits(data, len), None),
            DataType::Utf8 | DataType::Binary => {
                let (values, offsets) = varlen(data, len)?;
                (values, Some(offsets))
            }
            DataType::Interval => (intervals(data, len)?, None),
            DataType::Decimal128 { .. } => (decimals(data, len, flat.logical_type())?, None),
            other => {
                let width = other.width().ok_or_else(|| {
                    Error::internal(format!("{other:?} has no width and no buffer of its own"))
                })?;
                (fixed(data, len, width)?, None)
            }
        };
        Ok(Self { data_type, len, null_count, validity, offsets, values })
    }

    /// An array of this type with no values in it.
    ///
    /// A variable width type still gets its offsets buffer, holding the single zero that says the
    /// first value would start at the beginning. Arrow's rule is that offsets are one longer than
    /// the array, and an array of nothing is the case where forgetting it is easiest and where a
    /// reader that trusts the rule reads past the end.
    #[must_use]
    pub fn empty(data_type: DataType) -> Self {
        let offsets = (data_type.buffer_count() == 3).then(|| 0i32.to_le_bytes().to_vec());
        Self { data_type, len: 0, null_count: 0, validity: None, offsets, values: Vec::new() }
    }

    /// What the column holds.
    #[must_use]
    pub fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// How many values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many of them are null.
    #[must_use]
    pub fn null_count(&self) -> usize {
        self.null_count
    }

    /// The validity bitmap, or nothing when nothing is null.
    ///
    /// Nothing is what Arrow means by a null pointer in the first buffer slot, and it is the case
    /// worth keeping rather than materializing: a reader that sees it can skip the per value check
    /// for the whole array, which is the same reason `Validity::AllValid` exists on our side.
    #[must_use]
    pub fn validity(&self) -> Option<&[u8]> {
        self.validity.as_deref()
    }

    /// The offsets buffer, for the variable width types, as little endian `i32`.
    #[must_use]
    pub fn offsets(&self) -> Option<&[u8]> {
        self.offsets.as_deref()
    }

    /// The values buffer.
    #[must_use]
    pub fn values(&self) -> &[u8] {
        &self.values
    }

    /// The buffers in the order the C data interface lists them.
    ///
    /// Two or three of them, and which is which is the type's business rather than the caller's,
    /// which is why this exists next to the three accessors above.
    #[must_use]
    pub fn buffers(&self) -> Vec<Option<&[u8]>> {
        match self.data_type.buffer_count() {
            0 => Vec::new(),
            3 => vec![self.validity(), self.offsets(), Some(self.values())],
            _ => vec![self.validity(), Some(self.values())],
        }
    }
}

/// The validity bitmap as bytes, or nothing when there is nothing to say.
///
/// Our bitmap is already Arrow's: one bit per value, set meaning valid, least significant bit
/// first. So the only work is writing the words out little endian, and on a little endian machine
/// that is the bytes they already are.
fn bitmap(validity: &Validity, len: usize) -> Option<Vec<u8>> {
    if !validity.has_nulls(len) {
        return None;
    }
    let bytes = len.div_ceil(8);
    let mut out = vec![0u8; bytes];
    for index in 0..len {
        if validity.is_valid(index) {
            out[index / 8] |= 1 << (index % 8);
        }
    }
    Some(out)
}

/// A boolean column, which Arrow stores as a bit per value rather than a byte per value.
fn bits(data: &Data, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len.div_ceil(8)];
    if let Data::Bool(values) = data {
        for (index, &value) in values.as_slice().iter().take(len).enumerate() {
            if value {
                out[index / 8] |= 1 << (index % 8);
            }
        }
    }
    out
}

/// A string or blob column, as offsets and one run of bytes.
///
/// This is the copy that cannot be avoided. Our strings are views into an arena, in whatever order
/// they were written and with the short ones not in the arena at all, and Arrow's are back to back
/// in row order with an offset each.
fn varlen(data: &Data, len: usize) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut values = Vec::new();
    let mut offsets = Vec::with_capacity((len + 1) * 4);
    offsets.extend_from_slice(&0i32.to_le_bytes());
    for index in 0..len {
        if let Data::Varlen(column) = data {
            if let Some(bytes) = column.bytes(index) {
                values.extend_from_slice(bytes);
            }
        }
        // A null still gets an offset, and it is the same one as the value before it, which is what
        // makes a null and an empty string the same two numbers and the validity bitmap the only
        // thing that tells them apart. That is Arrow's rule and not a shortcut here.
        let so_far = i32::try_from(values.len()).map_err(|_| {
            Error::not_implemented(
                "a column of strings longer than two gigabytes, which 32 bit offsets cannot \
                 address, and which is what Arrow has LargeUtf8 for",
            )
        })?;
        offsets.extend_from_slice(&so_far.to_le_bytes());
    }
    Ok((values, offsets))
}

/// An interval column, as Arrow's month day nano triple.
///
/// Ours is months, days and microseconds, and Arrow's third field is nanoseconds, so the only
/// conversion is the factor of a thousand. It cannot overflow for any interval a query can produce:
/// the microseconds field is an `i64` and a thousand times it is still inside an `i128`, but Arrow
/// stores it in an `i64`, so an interval of more than about 292 years of microseconds saturates
/// rather than wrapping. DuckDB has the same limit from the same arithmetic.
fn intervals(data: &Data, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len * 16);
    if let Data::Interval(values) = data {
        for &(months, days, micros) in values.as_slice().iter().take(len) {
            out.extend_from_slice(&months.to_le_bytes());
            out.extend_from_slice(&days.to_le_bytes());
            out.extend_from_slice(&micros.saturating_mul(1_000).to_le_bytes());
        }
    }
    out.resize(len * 16, 0);
    Ok(out)
}

/// A decimal column, or a `HUGEINT` one, widened to the 128 bits Arrow stores a decimal in.
///
/// Our decimals live in the narrowest integer that holds the precision, which is what makes a
/// `DECIMAL(4, 2)` four times cheaper to add than a 128 bit one. Arrow has one decimal width, so
/// the export widens. `Data::signed_at` is the widening, and it is there rather than here because
/// four other places need the same five arms.
fn decimals(data: &Data, len: usize, ty: &LogicalType) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len * 16);
    for index in 0..len {
        let value = match data {
            Data::Empty => 0,
            _ => data.signed_at(index).ok_or_else(|| {
                Error::internal(format!("{ty} is stored as something that is not an integer"))
            })?,
        };
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
}

/// A fixed width column, as the bytes it already is.
fn fixed(data: &Data, len: usize, width: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len * width);
    macro_rules! pack {
        ($values:expr) => {
            for value in $values.as_slice().iter().take(len) {
                out.extend_from_slice(&value.to_le_bytes());
            }
        };
    }
    match data {
        // A vector where every value is null keeps no values at all, and Arrow still wants a buffer
        // of the right size under the bitmap that says to ignore it. The resize below writes it.
        Data::Empty => {}
        Data::Int8(values) => pack!(values),
        Data::Int16(values) => pack!(values),
        Data::Int32(values) => pack!(values),
        Data::Int64(values) => pack!(values),
        Data::Int128(values) => pack!(values),
        Data::UInt8(values) => pack!(values),
        Data::UInt16(values) => pack!(values),
        Data::UInt32(values) => pack!(values),
        Data::UInt64(values) => pack!(values),
        Data::UInt128(values) => pack!(values),
        Data::Float32(values) => pack!(values),
        Data::Float64(values) => pack!(values),
        other => {
            return Err(Error::internal(format!(
                "{other:?} is not a fixed width layout and reached the fixed width path"
            )));
        }
    }
    if out.len() > len * width {
        return Err(Error::internal(format!(
            "a column of {len} values of {width} bytes came to {} bytes",
            out.len()
        )));
    }
    out.resize(len * width, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::Vector;

    use super::{Array, DataType};
    use crate::types::TimeUnit;

    fn vector(ty: LogicalType, values: &[Value]) -> Vector {
        Vector::from_values(ty, values).expect("the values are of the type")
    }

    #[test]
    fn an_integer_column_is_four_little_endian_bytes_per_value() {
        let array = Array::of(&vector(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(-2), Value::Integer(3)],
        ))
        .expect("an integer maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Int32);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 0);
        assert_eq!(array.values(), &[1, 0, 0, 0, 254, 255, 255, 255, 3, 0, 0, 0]);
    }

    #[test]
    fn a_column_with_no_nulls_has_no_validity_bitmap_at_all() {
        let array = Array::of(&vector(LogicalType::BigInt, &[Value::BigInt(7)]))
            .expect("a bigint maps onto Arrow");
        assert_eq!(array.validity(), None);
        assert_eq!(array.buffers().len(), 2);
        assert_eq!(array.buffers()[0], None);
    }

    #[test]
    fn a_null_sets_its_bit_to_zero_and_leaves_the_value_slot_readable() {
        let array = Array::of(&vector(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        ))
        .expect("an integer maps onto Arrow");
        assert_eq!(array.null_count(), 1);
        // Bits 0 and 2 set, bit 1 clear, and the byte is padded with zeros up to eight bits.
        assert_eq!(array.validity(), Some(&[0b0000_0101u8][..]));
        // Arrow says the value under a null is undefined rather than absent, so the buffer is still
        // the full three values wide and a reader that ignores the bitmap reads something.
        assert_eq!(array.values().len(), 12);
    }

    #[test]
    fn a_boolean_column_is_packed_a_bit_per_value() {
        let values: Vec<Value> =
            [true, false, true, true, false, false, false, true, true].map(Value::Boolean).to_vec();
        let array =
            Array::of(&vector(LogicalType::Boolean, &values)).expect("a boolean maps onto Arrow");
        assert_eq!(array.len(), 9);
        assert_eq!(array.values(), &[0b1000_1101u8, 0b0000_0001]);
    }

    #[test]
    fn a_string_column_is_offsets_and_one_run_of_bytes() {
        let array = Array::of(&vector(
            LogicalType::Varchar,
            &[
                Value::Varchar("a".to_string()),
                Value::Varchar("bc".to_string()),
                Value::Varchar(String::new()),
            ],
        ))
        .expect("a varchar maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Utf8);
        assert_eq!(array.values(), b"abc");
        assert_eq!(offsets(&array), vec![0, 1, 3, 3]);
        assert_eq!(array.buffers().len(), 3);
    }

    #[test]
    fn a_null_string_gets_the_offset_of_the_one_before_it() {
        let array = Array::of(&vector(
            LogicalType::Varchar,
            &[Value::Varchar("ab".to_string()), Value::Null, Value::Varchar("c".to_string())],
        ))
        .expect("a varchar maps onto Arrow");
        // A null and an empty string are the same pair of offsets. The bitmap is the only thing
        // that tells them apart, which is Arrow's rule rather than a shortcut here.
        assert_eq!(offsets(&array), vec![0, 2, 2, 3]);
        assert_eq!(array.values(), b"abc");
        assert_eq!(array.validity(), Some(&[0b0000_0101u8][..]));
    }

    #[test]
    fn a_string_longer_than_the_inline_prefix_survives_the_arena() {
        let long = "the quick brown fox jumps over the lazy dog";
        let array = Array::of(&vector(LogicalType::Varchar, &[Value::Varchar(long.to_string())]))
            .expect("a varchar maps onto Arrow");
        assert_eq!(array.values(), long.as_bytes());
    }

    #[test]
    fn a_hugeint_is_widened_to_the_decimal_arrow_stores_it_in() {
        let array = Array::of(&vector(LogicalType::HugeInt, &[Value::HugeInt(-1)]))
            .expect("a hugeint maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Decimal128 { precision: 38, scale: 0 });
        assert_eq!(array.values(), &[0xff; 16]);
    }

    #[test]
    fn a_narrow_decimal_is_widened_to_sixteen_bytes_and_keeps_its_scale() {
        let array = Array::of(&vector(
            LogicalType::Decimal { width: 4, scale: 2 },
            &[Value::Decimal { unscaled: 1234, width: 4, scale: 2 }],
        ))
        .expect("a decimal maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Decimal128 { precision: 4, scale: 2 });
        assert_eq!(array.values().len(), 16);
        assert_eq!(i128::from_le_bytes(array.values().try_into().expect("sixteen bytes")), 1234);
    }

    #[test]
    fn an_interval_turns_its_microseconds_into_arrows_nanoseconds() {
        let array = Array::of(&vector(
            LogicalType::Interval,
            &[Value::Interval { months: 1, days: 2, micros: 3 }],
        ))
        .expect("an interval maps onto Arrow");
        assert_eq!(array.values()[0..4], 1i32.to_le_bytes());
        assert_eq!(array.values()[4..8], 2i32.to_le_bytes());
        assert_eq!(array.values()[8..16], 3_000i64.to_le_bytes());
    }

    #[test]
    fn a_timestamp_keeps_the_microseconds_it_already_counts_in() {
        let array = Array::of(&vector(LogicalType::Timestamp, &[Value::Timestamp(1_700_000)]))
            .expect("a timestamp maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Timestamp(TimeUnit::Microsecond, None));
        assert_eq!(array.values(), 1_700_000i64.to_le_bytes());
    }

    #[test]
    fn the_null_type_has_no_buffers_and_nothing_in_them() {
        let array = Array::of(&vector(LogicalType::Null, &[Value::Null, Value::Null]))
            .expect("the null type maps onto Arrow");
        assert_eq!(array.data_type(), &DataType::Null);
        assert_eq!(array.len(), 2);
        assert_eq!(array.null_count(), 2);
        assert!(array.buffers().is_empty());
        assert!(array.values().is_empty());
    }

    #[test]
    fn a_constant_vector_is_flattened_into_the_values_it_stands_for() {
        let array = Array::of(&Vector::constant(LogicalType::Integer, Value::Integer(9), 4))
            .expect("an integer maps onto Arrow");
        assert_eq!(array.len(), 4);
        assert_eq!(array.values(), &[9, 0, 0, 0, 9, 0, 0, 0, 9, 0, 0, 0, 9, 0, 0, 0]);
    }

    #[test]
    fn a_column_of_nothing_but_nulls_still_has_a_values_buffer_the_right_size() {
        let array = Array::of(&vector(LogicalType::BigInt, &[Value::Null, Value::Null]))
            .expect("a bigint maps onto Arrow");
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.values(), &[0u8; 16]);
        assert_eq!(array.validity(), Some(&[0u8][..]));
    }

    #[test]
    fn an_empty_string_array_still_carries_the_leading_offset() {
        let array = Array::empty(DataType::Utf8);
        assert!(array.is_empty());
        assert_eq!(array.offsets(), Some(&0i32.to_le_bytes()[..]));
        assert_eq!(array.buffers().len(), 3);
    }

    #[test]
    fn a_type_with_no_arrow_counterpart_is_refused_rather_than_guessed_at() {
        let error = Array::of(&Vector::constant(LogicalType::Uuid, Value::Null, 1))
            .expect_err("uuid has no Arrow type here yet");
        assert!(error.to_string().contains("UUID"), "{error}");
    }

    fn offsets(array: &Array) -> Vec<i32> {
        array
            .offsets()
            .expect("a variable width array has offsets")
            .chunks_exact(4)
            .map(|bytes| i32::from_le_bytes(bytes.try_into().expect("four bytes")))
            .collect()
    }
}
