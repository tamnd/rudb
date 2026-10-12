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
//! Each column chunk records its null count and, where the pin does, the smallest and the largest
//! value in both the old `min` and `max` fields and the newer `min_value` and `max_value`, in the
//! byte forms the reader's pruning reads. The pin keeps no bounds at all for a chunk that holds a
//! NaN, keeps only the newer fields for an unsigned integer, whose order the old ones do not have,
//! and keeps none for a `HUGEINT`, which it writes as a `DOUBLE`. A string or a blob bound is cut
//! to 256 bytes the way the pin cuts it and then says it is not exact. A `FLOAT` or a `DOUBLE`
//! chunk also records how many NaNs it holds, which is what the corpus expects of a DuckDB newer
//! than the pin. The footer says every column sorts by its type, and carries the key value pairs
//! `KV_METADATA` asks for.

use std::cmp::Ordering;
use std::io::Write;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_vector::{Chunk, Vector};

use crate::metadata::Physical;
use crate::thrift;

/// How many bytes of a string or a blob a bound keeps, which is the pin's limit.
const BOUND_BYTES: usize = 256;

/// One column as it is stored.
#[derive(Debug)]
struct Column {
    name: String,
    ty: LogicalType,
    physical: Physical,
    bounds: Bounds,
    /// Whether `RETURN_STATS` reports the column's bounds, which the pin does not for an enum.
    reported: bool,
}

/// Which bounds the pin keeps for a column, which follows the type the query gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bounds {
    /// The old fields and the new ones: a signed number, a bool, a date, a time or a decimal.
    Signed,
    /// Only the new fields, for an unsigned integer.
    Unsigned,
    /// Both, cut on a character past [`BOUND_BYTES`], for a string.
    Text,
    /// Both, cut on a byte past [`BOUND_BYTES`], for a blob.
    Bytes,
    /// None at all, for a `HUGEINT` or a `UHUGEINT`.
    None,
}

/// The per chunk totals the footer needs, filled in as the pages go out.
#[derive(Debug, Default)]
struct Written {
    values: i64,
    nulls: i64,
    nans: i64,
    pages: i32,
    compressed: i64,
    uncompressed: i64,
    min: Option<Value>,
    max: Option<Value>,
    /// Whether the minimum was cut short, which leaves it below the smallest value.
    min_cut: bool,
    /// Whether the maximum was cut short and raised, which leaves it above the largest value.
    max_cut: bool,
    /// Whether a long string could not be cut into an upper bound, which drops the bounds of the
    /// chunk for good.
    failed: bool,
}

impl Written {
    /// Moves the bounds out to take in a value that is neither null nor NaN.
    fn take(&mut self, value: &Value, bounds: Bounds) {
        match bounds {
            Bounds::None => {}
            Bounds::Text => self.take_text(value, true),
            Bounds::Bytes => self.take_text(value, false),
            Bounds::Signed | Bounds::Unsigned => {
                bound(&mut self.min, value, Ordering::Less);
                bound(&mut self.max, value, Ordering::Greater);
            }
        }
    }

    /// The pin's string statistics: a value past [`BOUND_BYTES`] that becomes a bound is cut, and
    /// later values are compared with the cut bound rather than with the value it came from.
    fn take_text(&mut self, value: &Value, text: bool) {
        if self.failed {
            return;
        }
        let bytes = match value {
            Value::Varchar(text) => text.as_bytes(),
            Value::Blob(bytes) => bytes.as_slice(),
            _ => return,
        };
        let make = |bytes: &[u8]| {
            if text {
                Value::Varchar(String::from_utf8_lossy(bytes).into_owned())
            } else {
                Value::Blob(bytes.to_vec())
            }
        };
        if self.min.as_ref().is_none_or(|min| bytes < text_bytes(min)) {
            self.min_cut = bytes.len() > BOUND_BYTES;
            self.min = Some(make(if self.min_cut { cut_low(bytes, text) } else { bytes }));
        }
        if self.max.as_ref().is_none_or(|max| bytes > text_bytes(max)) {
            self.max_cut = bytes.len() > BOUND_BYTES;
            if !self.max_cut {
                self.max = Some(make(bytes));
            } else if let Some(high) = cut_high(bytes, text) {
                self.max = Some(make(&high));
            } else {
                self.failed = true;
                self.min = None;
                self.max = None;
            }
        }
    }
}

/// The bytes of a string or a blob bound.
fn text_bytes(value: &Value) -> &[u8] {
    match value {
        Value::Varchar(text) => text.as_bytes(),
        Value::Blob(bytes) => bytes,
        _ => &[],
    }
}

/// A lower bound of `bytes` no longer than [`BOUND_BYTES`]: the front of it, which for a string
/// stops before a character that would be split.
fn cut_low(bytes: &[u8], text: bool) -> &[u8] {
    let mut end = BOUND_BYTES;
    while text && end > 0 && bytes[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    &bytes[..end]
}

/// An upper bound of `bytes` no longer than [`BOUND_BYTES`]: the front of it up to the last byte
/// that can be raised by one, raised. For a string that is an ASCII byte below `0x7F`, so the bound
/// stays whole characters, and for a blob any byte below `0xFF`. `None` when there is no such byte,
/// which is a string of nothing but long characters or a blob of nothing but `0xFF`.
fn cut_high(bytes: &[u8], text: bool) -> Option<Vec<u8>> {
    let ceiling = if text { 0x7F } else { 0xFF };
    let at = bytes[..BOUND_BYTES].iter().rposition(|&byte| byte < ceiling)?;
    let mut high = bytes[..=at].to_vec();
    high[at] += 1;
    Some(high)
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
    key_values: Vec<(String, Vec<u8>)>,
    totals: Vec<ColumnStatistics>,
    /// For each column, whether a row group has had no bounds, which leaves the file with none.
    unbounded: Vec<bool>,
}

/// What one column of a file holds over all of its row groups, which is what a `COPY ... TO`
/// with `RETURN_STATS` reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnStatistics {
    /// The name of the column.
    pub name: String,
    /// How many values it holds, the nulls among them.
    pub values: i64,
    /// How many of them are null.
    pub nulls: i64,
    /// How many are NaN, for a `FLOAT` or a `DOUBLE` column, and `None` for any other.
    pub nans: Option<i64>,
    /// How many bytes its column chunks take in the file, page headers and all.
    pub size: i64,
    /// The smallest value that is neither null nor NaN, of the type it was stored as, or `None`
    /// when a row group had no bounds. A string or a blob is cut the way its chunks' bounds are.
    pub min: Option<Value>,
    /// The largest.
    pub max: Option<Value>,
    /// Whether [`ColumnStatistics::min`] is the smallest value rather than one cut short.
    pub min_exact: bool,
    /// Whether [`ColumnStatistics::max`] is the largest value rather than one cut and raised.
    pub max_exact: bool,
}

/// The type a column of `ty` has to be cast to before it is handed to a [`Writer`].
///
/// Most types are written as themselves. An enum is written as its text and a timestamp in
/// seconds or milliseconds as one in microseconds, which is what the pin does with them, a
/// `HUGEINT` as a `DOUBLE`, which is also what the pin does, and an aggregate state as its layout.
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
        // A state is written as its layout, which is what the pin writes, and read back as that
        // layout, which `to_aggregate_state` turns into a state again.
        LogicalType::AggregateState(_) => storage(ty.storage())?,
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
    /// Starts a file with these columns, of the types the query gave them. The chunks handed in
    /// later have each column cast to what [`storage`] makes of its type.
    ///
    /// # Errors
    ///
    /// For a codec other than `UNCOMPRESSED` and `SNAPPY`, for a column of a type [`storage`]
    /// refuses, or if the magic cannot be written.
    pub fn new(out: W, fields: &[Field], codec: Codec, created_by: &str) -> Result<Self> {
        if !matches!(codec, Codec::Uncompressed | Codec::Snappy) {
            return Err(Error::not_implemented(format!(
                "COPY TO a Parquet file with the {codec:?} codec is not supported yet"
            )));
        }
        let mut columns = Vec::with_capacity(fields.len());
        for field in fields {
            let ty = storage(&field.ty)?;
            let physical = physical(&ty).ok_or_else(|| {
                Error::not_implemented(format!(
                    "COPY TO a Parquet file with a column of type {} is not supported yet",
                    field.ty
                ))
            })?;
            let bounds = match (&field.ty, &ty) {
                (LogicalType::HugeInt | LogicalType::UHugeInt, _) => Bounds::None,
                (
                    _,
                    LogicalType::UTinyInt
                    | LogicalType::USmallInt
                    | LogicalType::UInteger
                    | LogicalType::UBigInt,
                ) => Bounds::Unsigned,
                (_, LogicalType::Varchar) => Bounds::Text,
                (_, LogicalType::Blob) => Bounds::Bytes,
                _ => Bounds::Signed,
            };
            let reported = !matches!(field.ty, LogicalType::Enum(_));
            columns.push(Column { name: field.name.clone(), ty, physical, bounds, reported });
        }
        let totals = columns
            .iter()
            .map(|column| ColumnStatistics {
                name: column.name.clone(),
                nans: matches!(column.physical, Physical::Float | Physical::Double).then_some(0),
                min_exact: true,
                max_exact: true,
                ..ColumnStatistics::default()
            })
            .collect();
        let unbounded = columns.iter().map(|column| !column.reported).collect();
        let mut writer = Self {
            out,
            at: 0,
            columns,
            codec,
            groups: Vec::new(),
            rows: 0,
            created_by: created_by.to_string(),
            key_values: Vec::new(),
            totals,
            unbounded,
        };
        writer.put(b"PAR1")?;
        Ok(writer)
    }

    /// Sets the key value pairs the footer carries, which is what `KV_METADATA` writes.
    pub fn set_key_values(&mut self, pairs: Vec<(String, Vec<u8>)>) {
        self.key_values = pairs;
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
            let totals = &mut self.totals[column];
            totals.values += written.values;
            totals.nulls += written.nulls;
            totals.nans = totals.nans.map(|nans| nans + written.nans);
            totals.size += written.compressed;
            // A row group with no bounds leaves the file with none, as in the pin, which still
            // takes in the bounds of a group that kept none in the file only for its NaNs.
            match (&written.min, &written.max) {
                (Some(min), Some(max)) if !self.unbounded[column] => {
                    bound(&mut totals.min, min, Ordering::Less);
                    bound(&mut totals.max, max, Ordering::Greater);
                    totals.min_exact &= !written.min_cut;
                    totals.max_exact &= !written.max_cut;
                }
                _ => {
                    self.unbounded[column] = true;
                    totals.min = None;
                    totals.max = None;
                }
            }
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
        // row at a time: the Parquet writer encodes plain pages value by value; a writer that
        // takes a vector's flat data whole is not built yet.
        for row in 0..vector.len() {
            let value = vector.try_value_at(row)?;
            if value.is_null() {
                defined.push(false);
                written.nulls += 1;
                continue;
            }
            if matches!(value, Value::Double(value) if value.is_nan())
                || matches!(value, Value::Float(value) if value.is_nan())
            {
                written.nans += 1;
            } else {
                written.take(&value, column.bounds);
            }
            defined.push(true);
            if column.physical == Physical::Boolean {
                bits.push(matches!(value, Value::Boolean(true)));
            } else {
                plain(&value, column, &mut values, true)?;
            }
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
        written.pages += 1;
        written.uncompressed += (header.len() + body.len()) as i64;
        written.compressed += (header.len() + stored.len()) as i64;
        Ok(())
    }

    /// The `ColumnChunk` of one column of a row group.
    fn chunk_meta(&self, at: usize, first: u64, written: &Written) -> thrift::Writer {
        let column = &self.columns[at];
        let mut meta = thrift::Writer::default();
        meta.i32(1, physical_wire(column.physical));
        // The pin lists the encodings of the values only, which here is always `PLAIN`, and not
        // the `RLE` of the definition levels.
        meta.list_of_ints(2, &[0]);
        meta.list_of_strings(3, &[column.name.as_str()]);
        meta.i32(4, if self.codec == Codec::Snappy { 1 } else { 0 });
        meta.int(5, written.values);
        meta.int(6, written.uncompressed);
        meta.int(7, written.compressed);
        meta.int(9, first as i64);
        let mut bounds = None;
        if let (Some(min), Some(max), 0) = (&written.min, &written.max, written.nans) {
            let (mut low, mut high) = (Vec::new(), Vec::new());
            if plain(max, column, &mut high, false).is_ok()
                && plain(min, column, &mut low, false).is_ok()
            {
                bounds = Some((low, high));
            }
        }
        let mut stats = thrift::Writer::default();
        if let Some((low, high)) = &bounds
            && column.bounds != Bounds::Unsigned
        {
            stats.binary(1, high);
            stats.binary(2, low);
        }
        stats.int(3, written.nulls);
        if let Some((low, high)) = &bounds {
            stats.binary(5, high);
            stats.binary(6, low);
            stats.boolean(7, !written.max_cut);
            stats.boolean(8, !written.min_cut);
        }
        if matches!(column.physical, Physical::Float | Physical::Double) {
            stats.int(9, written.nans);
        }
        meta.nested(12, stats);
        // Every page is a version one data page of plain values.
        let mut pages = thrift::Writer::default();
        pages.i32(1, 0);
        pages.i32(2, 0);
        pages.i32(3, written.pages);
        meta.list_of_nested(13, vec![pages]);
        let mut chunk = thrift::Writer::default();
        chunk.int(2, first as i64);
        chunk.nested(3, meta);
        chunk
    }

    /// What each column holds over the row groups written so far, in the order of the columns.
    #[must_use]
    pub fn statistics(&self) -> &[ColumnStatistics] {
        &self.totals
    }

    /// How many row groups have been written so far.
    #[must_use]
    pub fn row_groups(&self) -> usize {
        self.groups.len()
    }

    /// Writes the footer and hands back what the file was written to.
    ///
    /// # Errors
    ///
    /// If the write fails.
    pub fn finish(self) -> Result<W> {
        Ok(self.finish_sized()?.0)
    }

    /// Writes the footer and hands back what the file was written to and how many bytes the
    /// footer's metadata took, which leaves out its length and the closing magic.
    ///
    /// # Errors
    ///
    /// If the write fails.
    pub fn finish_sized(mut self) -> Result<(W, u64)> {
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
        if !self.key_values.is_empty() {
            let pairs = self
                .key_values
                .iter()
                .map(|(key, value)| {
                    let mut pair = thrift::Writer::default();
                    pair.string(1, key);
                    pair.binary(2, value);
                    pair
                })
                .collect();
            footer.list_of_nested(5, pairs);
        }
        footer.string(6, &self.created_by);
        let orders = self
            .columns
            .iter()
            .map(|_| {
                let mut order = thrift::Writer::default();
                order.nested(1, thrift::Writer::default());
                order
            })
            .collect();
        footer.list_of_nested(7, orders);
        let footer = footer.stop();
        self.put(&footer)?;
        self.put(&(footer.len() as u32).to_le_bytes())?;
        self.put(b"PAR1")?;
        self.out
            .flush()
            .map_err(|error| Error::io(format!("could not write a parquet file: {error}")))?;
        Ok((self.out, footer.len() as u64))
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
            | Value::TimestampTz(value)
            | Value::TimestampS(value)
            | Value::TimestampMs(value)
            | Value::TimestampNs(value)
            | Value::TimeNs(value)
            | Value::TimestampTzNs(value),
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
        | Value::TimestampTz(value)
        | Value::TimestampS(value)
        | Value::TimestampMs(value)
        | Value::TimestampNs(value)
        | Value::TimeNs(value)
        | Value::TimestampTzNs(value) => i128::from(*value),
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
    use crate::footer::{MetadataTable, RawFooter};
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
    fn the_footer_keeps_the_pins_statistics_orders_and_keys() {
        let fields = vec![
            Field::new("d", LogicalType::Double),
            Field::new("u", LogicalType::UInteger),
            Field::new("h", LogicalType::HugeInt),
            Field::new("s", LogicalType::Varchar),
            Field::new("b", LogicalType::Blob),
        ];
        let long = "z".repeat(300);
        let chunk = Chunk::new(vec![
            column(LogicalType::Double, &[Value::Double(1.5), Value::Double(f64::NAN)]),
            column(LogicalType::UInteger, &[Value::UInteger(7), Value::UInteger(3)]),
            column(LogicalType::Double, &[Value::Double(2.0), Value::Double(1.0)]),
            column(LogicalType::Varchar, &[Value::Varchar("a".into()), Value::Varchar(long)]),
            column(LogicalType::Blob, &[Value::Blob(vec![0xFF; 300]), Value::Blob(vec![1])]),
        ])
        .expect("a chunk");
        let mut writer =
            Writer::new(Vec::new(), &fields, Codec::Uncompressed, "rudb test").expect("starts");
        writer.set_key_values(vec![("k".into(), b"v".to_vec())]);
        writer.write_group(&[chunk]).expect("writes");
        let totals = writer.statistics().to_vec();
        let bytes = writer.finish().expect("finishes");
        let len =
            u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().expect("4"));
        let tail = &bytes[bytes.len() - 8 - len as usize..bytes.len() - 8];
        let footer = RawFooter::parse(tail, bytes.len() as u64).expect("parses");
        let rows = MetadataTable::Columns.rows(&footer, "f.parquet").expect("rows");
        let (d, u, h, s, b) = (&rows[0], &rows[1], &rows[2], &rows[3], &rows[4]);
        assert_eq!((&d[10], &d[14], &d[15]), (&Value::Null, &Value::Null, &Value::Null));
        assert_eq!(d[32], Value::BigInt(1));
        assert_eq!(d[17], Value::Varchar("PLAIN".into()));
        assert_eq!(d[31].to_string(), "[{'page_type': DATA_PAGE, 'encoding': PLAIN, 'count': 1}]");
        assert_eq!((&u[10], &u[11]), (&Value::Null, &Value::Null));
        assert_eq!((&u[14], &u[15]), (&Value::UInteger(3), &Value::UInteger(7)));
        assert_eq!(u[32], Value::Null);
        assert_eq!((&h[14], &h[15], &h[32]), (&Value::Null, &Value::Null, &Value::BigInt(0)));
        let high = format!("{}{{", "z".repeat(255));
        assert_eq!((&s[14], &s[15]), (&Value::Varchar("a".into()), &Value::Varchar(high)));
        assert_eq!((&s[26], &s[27]), (&Value::Boolean(true), &Value::Boolean(false)));
        assert_eq!((&b[14], &b[15], &b[12]), (&Value::Null, &Value::Null, &Value::BigInt(0)));
        assert!(!totals[3].max_exact && totals[3].min_exact);
        assert_eq!((&totals[4].min, &totals[4].max), (&None, &None));
        assert_eq!(totals[0].nans, Some(1));
        let file = MetadataTable::File.rows(&footer, "f.parquet").expect("rows");
        let order = "'ColumnOrder(TYPE_ORDER=TypeDefinedOrder())'";
        assert_eq!(file[0][9].to_string(), format!("[{}]", [order; 5].join(", ")));
        let pairs = MetadataTable::KeyValue.rows(&footer, "f.parquet").expect("rows");
        assert_eq!(pairs[0][1..], [Value::Blob(b"k".to_vec()), Value::Blob(b"v".to_vec())]);
    }

    #[test]
    fn levels_are_one_run_or_packed() {
        assert_eq!(super::levels(&[true; 3]), vec![6, 1]);
        assert_eq!(super::levels(&[true, false, true]), vec![3, 0b101]);
    }
}
