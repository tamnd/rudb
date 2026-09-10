//! Column buffers, and the part that turns an Arrow array into one.
//!
//! The lab holds one chunk of one column at a time and hands it to the encoder. A chunk of strings
//! is one flat byte buffer plus the end offset of each value, not a vector of vectors, because a
//! chunk of `hits` is around a hundred thousand values wide and a hundred and five columns deep and
//! a separate allocation per value is thirteen million allocations per chunk. The slice of slices
//! the encoder wants is built once at encode time and thrown away.

use arrow_array::{Array, ArrayRef, cast::AsArray, types};
use arrow_schema::{DataType, TimeUnit};
use rudb_common::{Error, Result};

/// A chunk of one string or binary column.
#[derive(Debug, Default)]
pub struct ByteColumn {
    data: Vec<u8>,
    ends: Vec<u32>,
}

impl ByteColumn {
    pub fn push(&mut self, value: &[u8]) -> Result<()> {
        self.data.extend_from_slice(value);
        let end = u32::try_from(self.data.len()).map_err(|_| {
            Error::internal(format!("a chunk grew past 4 GB at {} bytes", self.data.len()))
        })?;
        self.ends.push(end);
        Ok(())
    }

    /// Total length of the values, which is the number the encoded size is a ratio of.
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.ends.clear();
    }

    /// The view the encoder takes. Rebuilt per chunk on purpose, see the module comment.
    pub fn values(&self) -> Vec<&[u8]> {
        let mut out = Vec::with_capacity(self.ends.len());
        let mut start = 0usize;
        for &end in &self.ends {
            let end = end as usize;
            out.push(&self.data[start..end]);
            start = end;
        }
        out
    }
}

/// A chunk of one integer column. Everything that is fixed width and not floating point ends up
/// here, including dates, times, timestamps and booleans, because that is what they are on disk and
/// the encoding question is about the bits.
#[derive(Debug, Default)]
pub struct IntColumn {
    values: Vec<i64>,
}

impl IntColumn {
    pub fn push(&mut self, value: i64) {
        self.values.push(value);
    }

    pub fn bytes(&self) -> usize {
        self.values.len() * 8
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    pub fn values(&self) -> &[i64] {
        &self.values
    }
}

/// What the lab does with a column, decided once from the Arrow type.
#[derive(Debug)]
pub enum Column {
    Bytes(ByteColumn),
    Ints(IntColumn),
    /// A type the encodings do not cover yet. Floating point is the whole of this case on the
    /// datasets M1 cares about, and section 6.2 leaves it for later on purpose.
    Skipped,
}

impl Column {
    pub fn for_type(data_type: &DataType) -> Self {
        match data_type {
            DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Utf8View
            | DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView
            | DataType::FixedSizeBinary(_) => Self::Bytes(ByteColumn::default()),
            DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Date32
            | DataType::Date64
            | DataType::Time32(_)
            | DataType::Time64(_)
            | DataType::Timestamp(_, _)
            | DataType::Decimal128(_, _) => Self::Ints(IntColumn::default()),
            _ => Self::Skipped,
        }
    }

    /// The size of the values as the lab counts them, which for strings is the bytes themselves and
    /// for anything fixed width is eight bytes a value however wide it was in the file. Counting a
    /// two byte column as eight is not a ratio anybody should quote, so the report prints the
    /// Parquet column size alongside and quotes that ratio instead.
    pub fn bytes(&self) -> usize {
        match self {
            Self::Bytes(column) => column.bytes(),
            Self::Ints(column) => column.bytes(),
            Self::Skipped => 0,
        }
    }

    pub fn clear(&mut self) {
        match self {
            Self::Bytes(column) => column.clear(),
            Self::Ints(column) => column.clear(),
            Self::Skipped => {}
        }
    }
}

/// How many values in this batch were null, and how many did not fit.
#[derive(Debug, Default, Clone, Copy)]
pub struct Losses {
    pub nulls: usize,
    /// Values that had to be changed to be stored. A `u64` above `i64::MAX` and a `Decimal128`
    /// outside the range of an `i64` are the two ways this happens, and both are counted rather
    /// than hidden because a column with a nonzero count here has a size in the report that is not
    /// a size for the real column.
    pub narrowed: usize,
}

impl Losses {
    fn merge(&mut self, other: Losses) {
        self.nulls += other.nulls;
        self.narrowed += other.narrowed;
    }
}

/// Append one Arrow array to one column buffer.
///
/// Nulls are appended as the empty string or as zero. That is not what the storage layer will do,
/// it will carry a validity bitmap next to the values, but it is the right thing for a size
/// experiment: it keeps the value count right so the per row numbers are comparable across columns,
/// and it costs a nearly free run of zeros or empties in whatever encoding wins.
pub fn append(column: &mut Column, array: &ArrayRef) -> Result<Losses> {
    let mut losses = Losses::default();
    match column {
        Column::Skipped => {}
        Column::Bytes(out) => losses.merge(append_bytes(out, array)?),
        Column::Ints(out) => losses.merge(append_ints(out, array)?),
    }
    Ok(losses)
}

fn append_bytes(out: &mut ByteColumn, array: &ArrayRef) -> Result<Losses> {
    let mut losses = Losses::default();
    macro_rules! copy {
        ($values:expr) => {{
            let values = $values;
            for index in 0..values.len() {
                if values.is_null(index) {
                    losses.nulls += 1;
                    out.push(b"")?;
                } else {
                    out.push(values.value(index).as_ref())?;
                }
            }
        }};
    }
    match array.data_type() {
        DataType::Utf8 => copy!(array.as_string::<i32>()),
        DataType::LargeUtf8 => copy!(array.as_string::<i64>()),
        DataType::Utf8View => copy!(array.as_string_view()),
        DataType::Binary => copy!(array.as_binary::<i32>()),
        DataType::LargeBinary => copy!(array.as_binary::<i64>()),
        DataType::BinaryView => copy!(array.as_binary_view()),
        DataType::FixedSizeBinary(_) => copy!(array.as_fixed_size_binary()),
        other => return Err(Error::internal(format!("{other} is not a byte column"))),
    }
    Ok(losses)
}

fn append_ints(out: &mut IntColumn, array: &ArrayRef) -> Result<Losses> {
    let mut losses = Losses::default();
    macro_rules! copy {
        ($arrow:ty) => {{
            let values = array.as_primitive::<$arrow>();
            for index in 0..values.len() {
                if values.is_null(index) {
                    losses.nulls += 1;
                    out.push(0);
                } else {
                    out.push(i64::from(values.value(index)));
                }
            }
        }};
    }
    // The wide unsigned and the decimal cases cannot use `from`, so they are written out.
    macro_rules! copy_wide {
        ($arrow:ty) => {{
            let values = array.as_primitive::<$arrow>();
            for index in 0..values.len() {
                if values.is_null(index) {
                    losses.nulls += 1;
                    out.push(0);
                } else {
                    match i64::try_from(values.value(index)) {
                        Ok(value) => out.push(value),
                        Err(_) => {
                            losses.narrowed += 1;
                            out.push(i64::MAX);
                        }
                    }
                }
            }
        }};
    }
    match array.data_type() {
        DataType::Boolean => {
            let values = array.as_boolean();
            for index in 0..values.len() {
                if values.is_null(index) {
                    losses.nulls += 1;
                    out.push(0);
                } else {
                    out.push(i64::from(values.value(index)));
                }
            }
        }
        DataType::Int8 => copy!(types::Int8Type),
        DataType::Int16 => copy!(types::Int16Type),
        DataType::Int32 => copy!(types::Int32Type),
        DataType::Int64 => copy!(types::Int64Type),
        DataType::UInt8 => copy!(types::UInt8Type),
        DataType::UInt16 => copy!(types::UInt16Type),
        DataType::UInt32 => copy!(types::UInt32Type),
        DataType::UInt64 => copy_wide!(types::UInt64Type),
        DataType::Date32 => copy!(types::Date32Type),
        DataType::Date64 => copy!(types::Date64Type),
        DataType::Time32(TimeUnit::Second) => copy!(types::Time32SecondType),
        DataType::Time32(TimeUnit::Millisecond) => copy!(types::Time32MillisecondType),
        DataType::Time64(TimeUnit::Microsecond) => copy!(types::Time64MicrosecondType),
        DataType::Time64(TimeUnit::Nanosecond) => copy!(types::Time64NanosecondType),
        DataType::Timestamp(TimeUnit::Second, _) => copy!(types::TimestampSecondType),
        DataType::Timestamp(TimeUnit::Millisecond, _) => copy!(types::TimestampMillisecondType),
        DataType::Timestamp(TimeUnit::Microsecond, _) => copy!(types::TimestampMicrosecondType),
        DataType::Timestamp(TimeUnit::Nanosecond, _) => copy!(types::TimestampNanosecondType),
        DataType::Decimal128(_, _) => copy_wide!(types::Decimal128Type),
        other => return Err(Error::internal(format!("{other} is not an integer column"))),
    }
    Ok(losses)
}

/// A short name for a type, for the report. The Arrow display of a timestamp carries a time zone
/// and a unit and is too wide for a table column.
pub fn short_name(data_type: &DataType) -> String {
    match data_type {
        DataType::Timestamp(unit, _) => format!("timestamp({unit:?})"),
        DataType::Decimal128(precision, scale) => format!("decimal({precision},{scale})"),
        DataType::FixedSizeBinary(width) => format!("binary({width})"),
        other => trim(&format!("{other}").to_lowercase()),
    }
}

/// A nested type displays as its whole shape, which for a map of structs of lists is wider than the
/// rest of the table put together. The lab does not encode those yet so the name only has to be
/// enough to recognise the column by.
fn trim(name: &str) -> String {
    const WIDEST: usize = 24;
    if name.len() <= WIDEST {
        return name.to_string();
    }
    let mut out: String = name.chars().take(WIDEST - 3).collect();
    out.push_str("...");
    out
}
