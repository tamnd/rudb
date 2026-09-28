//! The Parquet writer: a flat schema, `PLAIN` values, and `UNCOMPRESSED` or `SNAPPY` pages.
//!
//! What it writes is what the pin writes for the same columns, apart from the encodings. Every
//! column is `OPTIONAL`, the root of the schema is `duckdb_schema`, integers narrower than 32 bits
//! and every unsigned width carry both the old `converted_type` and the `logicalType` union, a
//! string is a `BYTE_ARRAY` annotated `UTF8`, a timestamp is microseconds, and a decimal is an
//! `INT32`, an `INT64` or a sixteen byte `FIXED_LEN_BYTE_ARRAY` by its width. So a file written
//! here reads back in the pin as the types it was written from, and in this crate's reader too.
//!
//! One data page per chunk handed in, each a version one page with its definition levels in the
//! hybrid encoding and its values plain. Dictionary pages and the delta encodings are how the pin
//! gets its files smaller, and they are the next step rather than this one.
//!
//! Each column chunk records the smallest and the largest value and the null count, in the byte
//! forms the reader's pruning reads, and says the bounds are exact, because nothing here shortens
//! them.

use std::cmp::Ordering;
use std::io::Write;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_vector::{Chunk, Vector};

use crate::metadata::Physical;
use crate::thrift;

/// One column as it is stored.
#[derive(Debug)]
struct Column {
    name: String,
    ty: LogicalType,
    physical: Physical,
}

/// The per chunk totals the footer needs, filled in as the pages go out.
#[derive(Debug, Default)]
struct Written {
    values: i64,
    nulls: i64,
    compressed: i64,
    uncompressed: i64,
    min: Option<Value>,
    max: Option<Value>,
}

/// Writes one Parquet file, a row group at a time.
#[derive(Debug)]
pub struct Writer<W: Write> {
    out: W,
    at: u64,
    columns: Vec<Column>,
    codec: Codec,
    groups: Vec<thrift::Writer>,
    rows: i64,
    created_by: String,
}

/// The type a column of `ty` has to be cast to before it is handed to a [`Writer`].
///
/// Most types are written as themselves. An enum is written as its text and a timestamp in
/// seconds or milliseconds as one in microseconds, which is what the pin does with them, and a
/// `HUGEINT` as a `DOUBLE`, which is also what the pin does.
///
/// # Errors
///
/// For a type this writer has no Parquet form for yet: the nested types, `UUID`, `INTERVAL`,
/// `BIT`, `TIME WITH TIME ZONE` and nanosecond timestamps.
pub fn storage(ty: &LogicalType) -> Result<LogicalType> {
    Ok(match ty {
        LogicalType::Enum(_) => LogicalType::Varchar,
        LogicalType::TimestampS | LogicalType::TimestampMs => LogicalType::Timestamp,
        LogicalType::HugeInt | LogicalType::UHugeInt => LogicalType::Double,
        LogicalType::Null => LogicalType::Integer,
        ty if physical(ty).is_some() => ty.clone(),
        other => {
            return Err(Error::not_implemented(format!(
                "COPY TO a Parquet file with a column of type {other} is not supported yet"
            )));
        }
    })
}

/// How a type this writer takes is stored.
fn physical(ty: &LogicalType) -> Option<Physical> {
    Some(match ty {
        LogicalType::Boolean => Physical::Boolean,
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::Date => Physical::Int32,
        LogicalType::BigInt
        | LogicalType::UBigInt
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz => Physical::Int64,
        LogicalType::Float => Physical::Float,
        LogicalType::Double => Physical::Double,
        LogicalType::Varchar | LogicalType::Blob => Physical::ByteArray,
        LogicalType::Decimal { width, .. } if *width <= 9 => Physical::Int32,
        LogicalType::Decimal { width, .. } if *width <= 18 => Physical::Int64,
        LogicalType::Decimal { .. } => Physical::FixedLenByteArray,
        _ => return None,
    })
}

/// The wire number of a physical type.
fn physical_wire(physical: Physical) -> i32 {
    match physical {
        Physical::Boolean => 0,
        Physical::Int32 => 1,
        Physical::Int64 => 2,
        Physical::Int96 => 3,
        Physical::Float => 4,
        Physical::Double => 5,
        Physical::ByteArray => 6,
        Physical::FixedLenByteArray => 7,
    }
}

impl<W: Write> Writer<W> {
    /// Starts a file with these columns, each of a type [`storage`] hands back.
    ///
    /// # Errors
    ///
    /// For a codec other than `UNCOMPRESSED` and `SNAPPY`, for a column of a type [`storage`]
    /// would have changed, or if the magic cannot be written.
    pub fn new(out: W, fields: &[Field], codec: Codec, created_by: &str) -> Result<Self> {
        if !matches!(codec, Codec::Uncompressed | Codec::Snappy) {
            return Err(Error::not_implemented(format!(
                "COPY TO a Parquet file with the {codec:?} codec is not supported yet"
            )));
        }
        let mut columns = Vec::with_capacity(fields.len());
        for field in fields {
            let physical = physical(&field.ty).ok_or_else(|| {
                Error::not_implemented(format!(
                    "COPY TO a Parquet file with a column of type {} is not supported yet",
                    field.ty
                ))
            })?;
            columns.push(Column { name: field.name.clone(), ty: field.ty.clone(), physical });
        }
        let mut writer = Self {
            out,
            at: 0,
            columns,
            codec,
            groups: Vec::new(),
            rows: 0,
            created_by: created_by.to_string(),
        };
        writer.put(b"PAR1")?;
        Ok(writer)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.out
            .write_all(bytes)
            .map_err(|error| Error::io(format!("could not write a parquet file: {error}")))?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// Writes these chunks as one row group, one page per chunk in every column.
    ///
    /// # Errors
    ///
    /// If a chunk does not have the columns the file was started with, or a write fails.
    pub fn write_group(&mut self, chunks: &[Chunk]) -> Result<()> {
        let rows: usize = chunks.iter().map(Chunk::len).sum();
        if rows == 0 {
            return Ok(());
        }
        let start = self.at;
        let mut columns = Vec::with_capacity(self.columns.len());
        let (mut total, mut compressed) = (0, 0);
        for column in 0..self.columns.len() {
            let first = self.at;
            let mut written = Written::default();
            for chunk in chunks {
                if chunk.width() != self.columns.len() {
                    return Err(Error::internal("a chunk with the wrong number of columns"));
                }
                self.page(column, chunk.column(column)?, &mut written)?;
            }
            total += written.uncompressed;
            compressed += written.compressed;
            columns.push(self.chunk_meta(column, first, &written));
        }
        let mut group = thrift::Writer::default();
        group.list_of_nested(1, columns);
        group.int(2, total);
        group.int(3, rows as i64);
        group.int(5, start as i64);
        group.int(6, compressed);
        self.groups.push(group);
        self.rows += rows as i64;
        Ok(())
    }

    /// One data page of one column.
    fn page(&mut self, at: usize, vector: &Vector, written: &mut Written) -> Result<()> {
        let column = &self.columns[at];
        let mut defined = Vec::with_capacity(vector.len());
        let mut values = Vec::new();
        let mut bits = Vec::new();
        for row in 0..vector.len() {
            let value = vector.try_value_at(row)?;
            if value.is_null() {
                defined.push(false);
                written.nulls += 1;
                continue;
            }
            defined.push(true);
            if column.physical == Physical::Boolean {
                bits.push(matches!(value, Value::Boolean(true)));
            } else {
                plain(&value, column, &mut values, true)?;
            }
            bound(&mut written.min, &value, Ordering::Less);
            bound(&mut written.max, &value, Ordering::Greater);
        }
        if column.physical == Physical::Boolean {
            values = packed(&bits);
        }
        let levels = levels(&defined);
        let mut body = Vec::with_capacity(4 + levels.len() + values.len());
        body.extend_from_slice(&(levels.len() as u32).to_le_bytes());
        body.extend_from_slice(&levels);
        body.extend_from_slice(&values);
        let stored = match self.codec {
            Codec::Snappy => rudb_compress::snappy::compress(&body),
            _ => body.clone(),
        };
        let mut data = thrift::Writer::default();
        data.i32(1, vector.len() as i32);
        data.i32(2, 0);
        data.i32(3, 3);
        data.i32(4, 3);
        let mut header = thrift::Writer::default();
        header.i32(1, 0);
        header.i32(2, i32::try_from(body.len()).map_err(|_| too_big())?);
        header.i32(3, i32::try_from(stored.len()).map_err(|_| too_big())?);
        header.nested(5, data);
        let header = header.stop();
        self.put(&header)?;
        self.put(&stored)?;
        written.values += vector.len() as i64;
        written.uncompressed += (header.len() + body.len()) as i64;
        written.compressed += (header.len() + stored.len()) as i64;
        Ok(())
    }

    /// The `ColumnChunk` of one column of a row group.
    fn chunk_meta(&self, at: usize, first: u64, written: &Written) -> thrift::Writer {
        let column = &self.columns[at];
        let mut meta = thrift::Writer::default();
        meta.i32(1, physical_wire(column.physical));
        meta.list_of_ints(2, &[0, 3]);
        meta.list_of_strings(3, &[column.name.as_str()]);
        meta.i32(4, if self.codec == Codec::Snappy { 1 } else { 0 });
        meta.int(5, written.values);
        meta.int(6, written.uncompressed);
        meta.int(7, written.compressed);
        meta.int(9, first as i64);
        let mut stats = thrift::Writer::default();
        stats.int(3, written.nulls);
        if let (Some(min), Some(max)) = (&written.min, &written.max) {
            let (mut low, mut high) = (Vec::new(), Vec::new());
            if plain(max, column, &mut high, false).is_ok()
                && plain(min, column, &mut low, false).is_ok()
            {
                stats.binary(5, &high);
                stats.binary(6, &low);
                stats.boolean(7, true);
                stats.boolean(8, true);
            }
        }
        meta.nested(12, stats);
        let mut chunk = thrift::Writer::default();
        chunk.int(2, first as i64);
        chunk.nested(3, meta);
        chunk
    }

    /// Writes the footer and hands back what the file was written to.
    ///
    /// # Errors
    ///
    /// If the write fails.
    pub fn finish(mut self) -> Result<W> {
        let mut schema = Vec::with_capacity(self.columns.len() + 1);
        let mut root = thrift::Writer::default();
        root.i32(3, 0);
        root.string(4, "duckdb_schema");
        root.i32(5, self.columns.len() as i32);
        schema.push(root);
        for column in &self.columns {
            schema.push(element(column));
        }
        let mut footer = thrift::Writer::default();
        footer.i32(1, 1);
        footer.list_of_nested(2, schema);
        footer.int(3, self.rows);
        footer.list_of_nested(4, std::mem::take(&mut self.groups));
        footer.string(6, &self.created_by);
        let footer = footer.stop();
        self.put(&footer)?;
        self.put(&(footer.len() as u32).to_le_bytes())?;
        self.put(b"PAR1")?;
        self.out
            .flush()
            .map_err(|error| Error::io(format!("could not write a parquet file: {error}")))?;
        Ok(self.out)
    }
}

fn too_big() -> Error {
    Error::not_implemented("a parquet page of more than 2 GiB")
}

/// The `SchemaElement` of one column.
fn element(column: &Column) -> thrift::Writer {
    let mut element = thrift::Writer::default();
    element.i32(1, physical_wire(column.physical));
    if column.physical == Physical::FixedLenByteArray {
        element.i32(2, 16);
    }
    element.i32(3, 1);
    element.string(4, &column.name);
    let micros = |utc: bool, field: i16| {
        let mut unit = thrift::Writer::default();
        unit.nested(2, thrift::Writer::default());
        let mut time = thrift::Writer::default();
        time.boolean(1, utc);
        time.nested(2, unit);
        (field, time)
    };
    // The pin writes the old annotation for everything and the new one only where the old one
    // cannot say it all, which is time zones and decimals.
    let (converted, logical) = match &column.ty {
        LogicalType::TinyInt => (Some(15), None),
        LogicalType::SmallInt => (Some(16), None),
        LogicalType::Integer => (Some(17), None),
        LogicalType::BigInt => (Some(18), None),
        LogicalType::UTinyInt => (Some(11), None),
        LogicalType::USmallInt => (Some(12), None),
        LogicalType::UInteger => (Some(13), None),
        LogicalType::UBigInt => (Some(14), None),
        LogicalType::Varchar => (Some(0), None),
        LogicalType::Date => (Some(6), None),
        LogicalType::Time => (Some(8), Some(micros(false, 7))),
        LogicalType::Timestamp => (Some(10), Some(micros(false, 8))),
        LogicalType::TimestampTz => (Some(10), Some(micros(true, 8))),
        LogicalType::Decimal { width, scale } => {
            let mut decimal = thrift::Writer::default();
            decimal.i32(1, i32::from(*scale));
            decimal.i32(2, i32::from(*width));
            (Some(5), Some((5, decimal)))
        }
        _ => (None, None),
    };
    if let Some(converted) = converted {
        element.i32(6, converted);
    }
    if let LogicalType::Decimal { width, scale } = &column.ty {
        element.i32(7, i32::from(*scale));
        element.i32(8, i32::from(*width));
    }
    if let Some((field, inner)) = logical {
        let mut union = thrift::Writer::default();
        union.nested(field, inner);
        element.nested(10, union);
    }
    element
}

/// Appends a value's plain encoding. `framed` puts the four byte length in front of a byte array,
/// which a page wants and a statistics bound does not.
fn plain(value: &Value, column: &Column, out: &mut Vec<u8>, framed: bool) -> Result<()> {
    match (value, column.physical) {
        (Value::Boolean(value), _) => out.push(u8::from(*value)),
        (Value::TinyInt(value), _) => out.extend_from_slice(&i32::from(*value).to_le_bytes()),
        (Value::SmallInt(value), _) => out.extend_from_slice(&i32::from(*value).to_le_bytes()),
        (Value::Integer(value) | Value::Date(value), _) => {
            out.extend_from_slice(&value.to_le_bytes());
        }
        (Value::UTinyInt(value), _) => out.extend_from_slice(&u32::from(*value).to_le_bytes()),
        (Value::USmallInt(value), _) => out.extend_from_slice(&u32::from(*value).to_le_bytes()),
        (Value::UInteger(value), _) => out.extend_from_slice(&value.to_le_bytes()),
        (
            Value::BigInt(value)
            | Value::Time(value)
            | Value::Timestamp(value)
            | Value::TimestampTz(value),
            _,
        ) => out.extend_from_slice(&value.to_le_bytes()),
        (Value::UBigInt(value), _) => out.extend_from_slice(&value.to_le_bytes()),
        (Value::Float(value), _) => out.extend_from_slice(&value.to_le_bytes()),
        (Value::Double(value), _) => out.extend_from_slice(&value.to_le_bytes()),
        (Value::Varchar(text), _) => byte_array(text.as_bytes(), out, framed),
        (Value::Blob(blob), _) => byte_array(blob, out, framed),
        (Value::Decimal { unscaled, .. }, Physical::Int32) => {
            out.extend_from_slice(&(*unscaled as i32).to_le_bytes());
        }
        (Value::Decimal { unscaled, .. }, Physical::Int64) => {
            out.extend_from_slice(&(*unscaled as i64).to_le_bytes());
        }
        (Value::Decimal { unscaled, .. }, _) => out.extend_from_slice(&unscaled.to_be_bytes()),
        (other, _) => {
            return Err(Error::internal(format!(
                "a {} value in the parquet column {} of type {}",
                other.logical_type(),
                column.name,
                column.ty
            )));
        }
    }
    Ok(())
}

fn byte_array(bytes: &[u8], out: &mut Vec<u8>, framed: bool) {
    if framed {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(bytes);
}

/// Moves `bound` to `value` when `value` lies further out on the side `side` says. A NaN is left
/// out of the bounds, the way the format asks.
fn bound(bound: &mut Option<Value>, value: &Value, side: Ordering) {
    if matches!(value, Value::Double(value) if value.is_nan())
        || matches!(value, Value::Float(value) if value.is_nan())
    {
        return;
    }
    let further = bound.as_ref().is_none_or(|current| compare(value, current) == side);
    if further {
        *bound = Some(value.clone());
    }
}

/// The order the reader's pruning compares bounds in, for two values of one column.
fn compare(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => left.total_cmp(right),
        (Value::Double(left), Value::Double(right)) => left.total_cmp(right),
        (Value::Varchar(left), Value::Varchar(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::Blob(left), Value::Blob(right)) => left.cmp(right),
        (Value::Decimal { unscaled: left, .. }, Value::Decimal { unscaled: right, .. }) => {
            left.cmp(right)
        }
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => integer(left).cmp(&integer(right)),
    }
}

/// An integer-like value widened, for [`compare`].
fn integer(value: &Value) -> i128 {
    match value {
        Value::TinyInt(value) => i128::from(*value),
        Value::SmallInt(value) => i128::from(*value),
        Value::Integer(value) | Value::Date(value) => i128::from(*value),
        Value::UTinyInt(value) => i128::from(*value),
        Value::USmallInt(value) => i128::from(*value),
        Value::UInteger(value) => i128::from(*value),
        Value::BigInt(value)
        | Value::Time(value)
        | Value::Timestamp(value)
        | Value::TimestampTz(value) => i128::from(*value),
        Value::UBigInt(value) => i128::from(*value),
        _ => 0,
    }
}

/// Booleans bit packed, the first in the lowest bit.
fn packed(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0_u8; bits.len().div_ceil(8)];
    for (at, bit) in bits.iter().enumerate() {
        if *bit {
            out[at / 8] |= 1 << (at % 8);
        }
    }
    out
}

/// Definition levels of bit width one in the hybrid encoding: one run when every value is there,
/// and the levels bit packed eight at a time when some are not.
fn levels(defined: &[bool]) -> Vec<u8> {
    let mut out = Vec::new();
    let varint = |mut value: u64, out: &mut Vec<u8>| {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    };
    if defined.iter().all(|defined| *defined) {
        varint((defined.len() as u64) << 1, &mut out);
        out.push(1);
        return out;
    }
    let groups = defined.len().div_ceil(8);
    varint(((groups as u64) << 1) | 1, &mut out);
    out.extend_from_slice(&packed(defined));
    out
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};
    use rudb_compress::Codec;
    use rudb_vector::{Chunk, Vector};

    use super::Writer;
    use crate::metadata::{Metadata, Physical};

    fn column(ty: LogicalType, values: &[Value]) -> Vector {
        Vector::from_values(ty, values).expect("builds")
    }

    #[test]
    fn a_written_footer_reads_back_with_its_types_and_bounds() {
        let fields = vec![
            Field::new("i", LogicalType::Integer),
            Field::new("s", LogicalType::Varchar),
            Field::new("d", LogicalType::Decimal { width: 10, scale: 2 }),
            Field::new("u", LogicalType::UTinyInt),
            Field::new("b", LogicalType::Boolean),
        ];
        for codec in [Codec::Uncompressed, Codec::Snappy] {
            let mut writer = Writer::new(Vec::new(), &fields, codec, "rudb test").expect("starts");
            let chunk = Chunk::new(vec![
                column(LogicalType::Integer, &[Value::Integer(5), Value::Null, Value::Integer(-2)]),
                column(
                    LogicalType::Varchar,
                    &[Value::Varchar("b".into()), Value::Varchar("a".into()), Value::Null],
                ),
                column(
                    LogicalType::Decimal { width: 10, scale: 2 },
                    &[
                        Value::Decimal { unscaled: 150, width: 10, scale: 2 },
                        Value::Null,
                        Value::Decimal { unscaled: -1, width: 10, scale: 2 },
                    ],
                ),
                column(
                    LogicalType::UTinyInt,
                    &[Value::UTinyInt(200), Value::UTinyInt(1), Value::Null],
                ),
                column(
                    LogicalType::Boolean,
                    &[Value::Boolean(true), Value::Boolean(false), Value::Boolean(true)],
                ),
            ])
            .expect("a chunk");
            writer.write_group(std::slice::from_ref(&chunk)).expect("writes");
            writer.write_group(&[chunk]).expect("writes");
            let bytes = writer.finish().expect("finishes");
            assert_eq!(&bytes[..4], b"PAR1");
            assert_eq!(&bytes[bytes.len() - 4..], b"PAR1");
            let len =
                u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().expect("4"));
            let footer = &bytes[bytes.len() - 8 - len as usize..bytes.len() - 8];
            let metadata = Metadata::parse(footer).expect("parses");
            assert_eq!(metadata.rows, 6);
            assert_eq!(metadata.row_groups.len(), 2);
            let types: Vec<_> = metadata.schema.iter().map(|column| column.ty.clone()).collect();
            assert_eq!(
                types,
                vec![
                    LogicalType::Integer,
                    LogicalType::Varchar,
                    LogicalType::Decimal { width: 10, scale: 2 },
                    LogicalType::UTinyInt,
                    LogicalType::Boolean,
                ]
            );
            assert_eq!(metadata.schema[2].physical, Physical::Int64);
            let stats = metadata.row_groups[0].columns[0].stats.clone().expect("stats");
            assert_eq!(stats.nulls, Some(1));
            assert_eq!(stats.min, Some((-2_i32).to_le_bytes().to_vec()));
            assert_eq!(stats.max, Some(5_i32.to_le_bytes().to_vec()));
            let stats = metadata.row_groups[0].columns[1].stats.clone().expect("stats");
            assert_eq!(stats.min, Some(b"a".to_vec()));
            assert_eq!(stats.max, Some(b"b".to_vec()));
            let stats = metadata.row_groups[0].columns[3].stats.clone().expect("stats");
            assert_eq!(stats.min, Some(1_u32.to_le_bytes().to_vec()));
            assert_eq!(stats.max, Some(200_u32.to_le_bytes().to_vec()));
        }
    }

    #[test]
    fn levels_are_one_run_or_packed() {
        assert_eq!(super::levels(&[true; 3]), vec![6, 1]);
        assert_eq!(super::levels(&[true, false, true]), vec![3, 0b101]);
    }
}
