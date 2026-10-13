//! The Parquet writer: a flat schema, dictionary or `PLAIN` values, and `UNCOMPRESSED` or `SNAPPY`
//! pages.
//!
//! What it writes is what the pin writes for the same columns. Every
//! column is `OPTIONAL`, the root of the schema is `duckdb_schema`, integers narrower than 32 bits
//! and every unsigned width carry both the old `converted_type` and the `logicalType` union, a
//! string is a `BYTE_ARRAY` annotated `UTF8`, a timestamp is microseconds, and a decimal is an
//! `INT32`, an `INT64` or a sixteen byte `FIXED_LEN_BYTE_ARRAY` by its width. So a file written
//! here reads back in the pin as the types it was written from, and in this crate's reader too.
//!
//! The pages are laid out the way the pin lays them out. Each column of a row group is read whole
//! first and, unless it is a boolean or a sixteen byte decimal, put in a dictionary that takes the
//! first of each value until there would be more entries than a fifth of the rows. A column that
//! fits is written as a plain dictionary page and then version one `PLAIN_DICTIONARY` data pages
//! of indices, and one that does not, or has no value at all, as version one `PLAIN` pages. An
//! enum always has a dictionary of every member and `RLE_DICTIONARY` pages. A data page is closed
//! once the estimated size of its values reaches the data page limit, which by default is so high
//! that a column chunk is one page. Levels and indices are written in the hybrid encoding with the
//! pin's choice of runs, so a file comes out the same bytes as the pin's for the same rows. The
//! delta encodings of the version two pages are the next step.
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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_compress::Codec;
use rudb_vector::Chunk;

use crate::metadata::Physical;
use crate::runs::{Runs, index_width};
use crate::thrift;

/// How many bytes of a string or a blob a bound keeps, which is the pin's limit.
const BOUND_BYTES: usize = 256;

/// The value encodings this writer uses, by their wire numbers.
const PLAIN: i32 = 0;
const PLAIN_DICTIONARY: i32 = 2;
const RLE_DICTIONARY: i32 = 8;

/// How large the pin lets a dictionary and a data page grow, which `COPY ... TO` sets with
/// `DICTIONARY_SIZE_LIMIT`, `STRING_DICTIONARY_PAGE_SIZE_LIMIT` and `DATA_PAGE_SIZE_LIMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most values a dictionary holds, where `None` is a fifth of the rows of the row group
    /// and zero means no dictionary at all.
    pub dictionary: Option<u64>,
    /// The most bytes the values of a string or a blob dictionary take, lengths and all.
    pub string_dictionary: u64,
    /// The estimated size of its values at which a data page is closed.
    pub data_page: u64,
}

impl Limits {
    /// The largest `STRING_DICTIONARY_PAGE_SIZE_LIMIT`, which is also the default.
    pub const MOST_STRING_DICTIONARY: u64 = 1 << 30;
    /// The largest `DATA_PAGE_SIZE_LIMIT`, which is also the default.
    pub const MOST_DATA_PAGE: u64 = 100 << 20;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            dictionary: None,
            string_dictionary: Self::MOST_STRING_DICTIONARY,
            data_page: Self::MOST_DATA_PAGE,
        }
    }
}

/// One column as it is stored.
#[derive(Debug)]
struct Column {
    name: String,
    ty: LogicalType,
    physical: Physical,
    bounds: Bounds,
    /// Whether `RETURN_STATS` reports the column's bounds, which the pin does not for an enum.
    reported: bool,
    /// Whether the values can go in a dictionary, which in the pin is every type but a boolean
    /// and a sixteen byte decimal.
    dictionary: bool,
    /// For an enum, its members.
    members: Option<Members>,
}

/// The members of an enum column, which the pin writes through a dictionary of its own.
#[derive(Debug)]
struct Members {
    labels: Arc<[String]>,
    index: HashMap<String, u32>,
    /// Whether a row has held each member so far, which the pin keeps over the whole file.
    seen: Vec<bool>,
}

/// One column of a row group, read in full before any of it is written, because whether a
/// dictionary holds it is only known once every value has been seen.
#[derive(Debug, Default)]
struct Gathered {
    /// Whether each row holds a value.
    defined: Vec<bool>,
    /// The plain encoding of each value, framed, one after another. A boolean's is one byte.
    plain: Vec<u8>,
    /// Where each value's encoding ends in `plain`.
    ends: Vec<usize>,
    /// The values as dictionary indices, when a dictionary holds them.
    indices: Vec<u32>,
    /// The body of the dictionary page and how many values it holds, when there is one.
    dictionary: Option<(Vec<u8>, usize)>,
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
    limits: Limits,
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
            let dictionary = !matches!(physical, Physical::Boolean | Physical::FixedLenByteArray);
            let members = match &field.ty {
                LogicalType::Enum(labels) => Some(Members {
                    labels: Arc::clone(labels),
                    index: labels.iter().zip(0..).map(|(label, at)| (label.clone(), at)).collect(),
                    seen: vec![false; labels.len()],
                }),
                _ => None,
            };
            columns.push(Column {
                name: field.name.clone(),
                ty,
                physical,
                bounds,
                reported,
                dictionary,
                members,
            });
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
            limits: Limits::default(),
        };
        writer.put(b"PAR1")?;
        Ok(writer)
    }

    /// Sets the key value pairs the footer carries, which is what `KV_METADATA` writes.
    pub fn set_key_values(&mut self, pairs: Vec<(String, Vec<u8>)>) {
        self.key_values = pairs;
    }

    /// Sets how large a dictionary and a data page may grow.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.out
            .write_all(bytes)
            .map_err(|error| Error::io(format!("could not write a parquet file: {error}")))?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// Writes these chunks as one row group.
    ///
    /// # Errors
    ///
    /// If a chunk does not have the columns the file was started with, or a write fails.
    pub fn write_group(&mut self, chunks: &[Chunk]) -> Result<()> {
        let rows: usize = chunks.iter().map(Chunk::len).sum();
        if rows == 0 {
            return Ok(());
        }
        if chunks.iter().any(|chunk| chunk.width() != self.columns.len()) {
            return Err(Error::internal("a chunk with the wrong number of columns"));
        }
        let start = self.at;
        let mut columns = Vec::with_capacity(self.columns.len());
        let (mut total, mut compressed) = (0, 0);
        for column in 0..self.columns.len() {
            let mut written = Written::default();
            let gathered = self.gather(column, chunks, rows, &mut written)?;
            columns.push(self.write_chunk(column, &gathered, &mut written)?);
            total += written.uncompressed;
            compressed += written.compressed;
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

    /// Reads one column of a row group, takes in its statistics, and builds the dictionary the
    /// pin would: one entry for each value the first time it is seen, until there would be more
    /// than a fifth of the rows of the group, or than `DICTIONARY_SIZE_LIMIT`, or for strings and
    /// blobs more bytes than `STRING_DICTIONARY_PAGE_SIZE_LIMIT`. A column that outgrows its
    /// dictionary, or has no value at all, is written plain. A boolean or a sixteen byte decimal
    /// never has one, and an enum always has its own, which lists every member.
    fn gather(
        &mut self,
        at: usize,
        chunks: &[Chunk],
        rows: usize,
        written: &mut Written,
    ) -> Result<Gathered> {
        let limits = self.limits;
        let column = &mut self.columns[at];
        let mut gathered = Gathered { defined: Vec::with_capacity(rows), ..Gathered::default() };
        let most = limits.dictionary.unwrap_or(rows as u64 / 5);
        let mut entries: HashMap<Vec<u8>, u32> = HashMap::new();
        let mut page = Vec::new();
        let mut open = column.dictionary;
        let text = column.physical == Physical::ByteArray;
        for chunk in chunks {
            let vector = chunk.column(at)?;
            // row at a time: the Parquet writer encodes values one by one; a writer that takes a
            // vector's flat data whole is not built yet.
            for row in 0..vector.len() {
                let value = vector.try_value_at(row)?;
                if value.is_null() {
                    gathered.defined.push(false);
                    written.nulls += 1;
                    continue;
                }
                gathered.defined.push(true);
                if is_nan(&value) {
                    written.nans += 1;
                } else if column.members.is_none() {
                    written.take(&value, column.bounds);
                }
                let begin = gathered.plain.len();
                plain(&value, column, &mut gathered.plain, true)?;
                gathered.ends.push(gathered.plain.len());
                if let Some(members) = &mut column.members {
                    let index = match &value {
                        Value::Varchar(label) => members.index.get(label.as_str()).copied(),
                        _ => None,
                    };
                    let index = index.ok_or_else(|| {
                        Error::internal(format!("{value} is not a member of the enum written"))
                    })?;
                    members.seen[index as usize] = true;
                    gathered.indices.push(index);
                } else if open {
                    let bytes = &gathered.plain[begin..];
                    let key = dictionary_key(bytes, column.physical);
                    if let Some(&index) = entries.get(key.as_ref()) {
                        gathered.indices.push(index);
                    } else if entries.len() as u64 + 1 > most
                        || (text && (page.len() + bytes.len()) as u64 > limits.string_dictionary)
                    {
                        open = false;
                    } else {
                        let index = entries.len() as u32;
                        entries.insert(key.into_owned(), index);
                        page.extend_from_slice(bytes);
                        gathered.indices.push(index);
                    }
                }
            }
        }
        if let Some(members) = &column.members {
            // The members no row has held are in the dictionary as empty strings, and the
            // bounds are those of the members held so far in the file, which is the pin's.
            let mut page = Vec::new();
            for (label, seen) in members.labels.iter().zip(&members.seen) {
                if *seen {
                    written.take(&Value::Varchar(label.clone()), column.bounds);
                    byte_array(label.as_bytes(), &mut page, true);
                } else {
                    page.extend_from_slice(&0_u32.to_le_bytes());
                }
            }
            gathered.dictionary = Some((page, members.labels.len()));
        } else if open && !entries.is_empty() {
            gathered.dictionary = Some((page, entries.len()));
        } else {
            gathered.indices = Vec::new();
        }
        Ok(gathered)
    }

    /// Writes one column of a row group, its dictionary page first when it has one, and hands
    /// back its `ColumnChunk`.
    fn write_chunk(
        &mut self,
        at: usize,
        gathered: &Gathered,
        written: &mut Written,
    ) -> Result<thrift::Writer> {
        let physical = self.columns[at].physical;
        let enumerated = self.columns[at].members.is_some();
        let (encoding, width) = match &gathered.dictionary {
            Some((_, count)) if enumerated => (RLE_DICTIONARY, index_width(*count)),
            Some((_, count)) => (PLAIN_DICTIONARY, index_width(*count)),
            None => (PLAIN, 0),
        };
        let mut dictionary = None;
        if let Some((body, count)) = &gathered.dictionary {
            dictionary = Some((self.at, *count));
            let mut header = thrift::Writer::default();
            header.i32(1, i32::try_from(*count).map_err(|_| too_big())?);
            header.i32(2, PLAIN);
            self.page(2, body, (7, header), written)?;
        }
        let first = self.at;
        for (rows, values) in pages(gathered, width, self.limits.data_page) {
            let mut levels = Runs::new(1);
            for defined in &gathered.defined[rows.clone()] {
                levels.push(u32::from(*defined));
            }
            let levels = levels.finish();
            let mut body = Vec::with_capacity(4 + levels.len());
            body.extend_from_slice(&(levels.len() as u32).to_le_bytes());
            body.extend_from_slice(&levels);
            if encoding != PLAIN {
                body.push(width);
                if !values.is_empty() {
                    let mut indices = Runs::new(width);
                    for index in &gathered.indices[values] {
                        indices.push(*index);
                    }
                    body.extend_from_slice(&indices.finish());
                }
            } else {
                let start = values.start.checked_sub(1).map_or(0, |at| gathered.ends[at]);
                let end = values.end.checked_sub(1).map_or(0, |at| gathered.ends[at]);
                if physical == Physical::Boolean {
                    body.extend_from_slice(&packed(&gathered.plain[start..end]));
                } else {
                    body.extend_from_slice(&gathered.plain[start..end]);
                }
            }
            let mut data = thrift::Writer::default();
            data.i32(1, rows.len() as i32);
            data.i32(2, encoding);
            data.i32(3, 3);
            data.i32(4, 3);
            self.page(0, &body, (5, data), written)?;
            written.pages += 1;
        }
        written.values += gathered.defined.len() as i64;
        Ok(self.chunk_meta(at, first, dictionary, encoding, written))
    }

    /// Writes one page: its header, of page type `kind` with the type's own header in `inner`,
    /// and then its body, compressed.
    fn page(
        &mut self,
        kind: i32,
        body: &[u8],
        inner: (i16, thrift::Writer),
        written: &mut Written,
    ) -> Result<()> {
        let stored = match self.codec {
            Codec::Snappy => Cow::Owned(rudb_compress::snappy::compress(body)),
            _ => Cow::Borrowed(body),
        };
        let mut header = thrift::Writer::default();
        header.i32(1, kind);
        header.i32(2, i32::try_from(body.len()).map_err(|_| too_big())?);
        header.i32(3, i32::try_from(stored.len()).map_err(|_| too_big())?);
        header.nested(inner.0, inner.1);
        let header = header.stop();
        self.put(&header)?;
        self.put(&stored)?;
        written.uncompressed += (header.len() + body.len()) as i64;
        written.compressed += (header.len() + stored.len()) as i64;
        Ok(())
    }

    /// The `ColumnChunk` of one column of a row group whose first data page is at `first`, with
    /// where its dictionary page is and how many values it holds when it has one.
    fn chunk_meta(
        &self,
        at: usize,
        first: u64,
        dictionary: Option<(u64, usize)>,
        encoding: i32,
        written: &Written,
    ) -> thrift::Writer {
        let column = &self.columns[at];
        let mut meta = thrift::Writer::default();
        meta.i32(1, physical_wire(column.physical));
        // The pin lists the encodings of the values only, and not the `RLE` of the definition
        // levels.
        meta.list_of_ints(2, &[i64::from(encoding)]);
        meta.list_of_strings(3, &[column.name.as_str()]);
        meta.i32(4, if self.codec == Codec::Snappy { 1 } else { 0 });
        meta.int(5, written.values);
        meta.int(6, written.uncompressed);
        meta.int(7, written.compressed);
        meta.int(9, first as i64);
        if let Some((offset, _)) = dictionary {
            meta.int(11, offset as i64);
        }
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
        if let Some((_, count)) = dictionary {
            stats.int(4, count as i64);
        }
        if let Some((low, high)) = &bounds {
            // The pin never calls the bounds of an enum exact.
            let enumerated = column.members.is_some();
            stats.binary(5, high);
            stats.binary(6, low);
            stats.boolean(7, !written.max_cut && !enumerated);
            stats.boolean(8, !written.min_cut && !enumerated);
        }
        if matches!(column.physical, Physical::Float | Physical::Double) {
            stats.int(9, written.nans);
        }
        meta.nested(12, stats);
        let count = |kind: i32, encoding: i32, count: i32| {
            let mut pages = thrift::Writer::default();
            pages.i32(1, kind);
            pages.i32(2, encoding);
            pages.i32(3, count);
            pages
        };
        let mut counts = Vec::with_capacity(2);
        if dictionary.is_some() {
            counts.push(count(2, PLAIN, 1));
        }
        counts.push(count(0, encoding, written.pages));
        meta.list_of_nested(13, counts);
        // The pin leaves the chunk's own offset at zero and says where it is in the metadata.
        let mut chunk = thrift::Writer::default();
        chunk.int(2, 0);
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

/// Booleans of a byte each bit packed, the first in the lowest bit.
fn packed(bools: &[u8]) -> Vec<u8> {
    let mut out = vec![0_u8; bools.len().div_ceil(8)];
    for (at, bit) in bools.iter().enumerate() {
        if *bit != 0 {
            out[at / 8] |= 1 << (at % 8);
        }
    }
    out
}

/// Whether a value is a NaN.
fn is_nan(value: &Value) -> bool {
    matches!(value, Value::Double(value) if value.is_nan())
        || matches!(value, Value::Float(value) if value.is_nan())
}

/// The bytes a dictionary tells values apart by, which are their plain encoding, except that
/// every NaN is one value and so are both zeros, which is how the pin compares floats.
fn dictionary_key(bytes: &[u8], physical: Physical) -> Cow<'_, [u8]> {
    match physical {
        Physical::Float => match <[u8; 4]>::try_from(bytes).map(f32::from_le_bytes) {
            Ok(value) if value.is_nan() => Cow::Owned(f32::NAN.to_le_bytes().to_vec()),
            Ok(0.0) => Cow::Owned(0_f32.to_le_bytes().to_vec()),
            _ => Cow::Borrowed(bytes),
        },
        Physical::Double => match <[u8; 8]>::try_from(bytes).map(f64::from_le_bytes) {
            Ok(value) if value.is_nan() => Cow::Owned(f64::NAN.to_le_bytes().to_vec()),
            Ok(0.0) => Cow::Owned(0_f64.to_le_bytes().to_vec()),
            _ => Cow::Borrowed(bytes),
        },
        _ => Cow::Borrowed(bytes),
    }
}

/// Where the pin ends the data pages of a column chunk, as the rows and the values each one
/// holds. There is one page unless the estimated size of the values on it reaches `limit`, which
/// closes it after the value that did. A value is estimated at its plain size, which for a
/// boolean is a byte, or with a dictionary at the whole bytes its index takes. A null adds
/// nothing.
fn pages(gathered: &Gathered, width: u8, limit: u64) -> Vec<(Range<usize>, Range<usize>)> {
    let mut pages = Vec::new();
    let (mut row, mut value, mut size, mut values) = (0, 0, 0_u64, 0_usize);
    for (at, defined) in gathered.defined.iter().enumerate() {
        if !*defined {
            continue;
        }
        size += if width == 0 {
            let start = values.checked_sub(1).map_or(0, |last| gathered.ends[last]);
            (gathered.ends[values] - start) as u64
        } else {
            u64::from(width.div_ceil(8))
        };
        values += 1;
        if size >= limit {
            pages.push((row..at + 1, value..values));
            (row, value, size) = (at + 1, values, 0);
        }
    }
    if row < gathered.defined.len() {
        pages.push((row..gathered.defined.len(), value..values));
    }
    pages
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

    /// The offsets and sizes of each column chunk of a file: the dictionary page, the first data
    /// page, the bytes it takes and the bytes it would take uncompressed.
    fn layout(bytes: &[u8]) -> Vec<[Value; 4]> {
        let len =
            u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().expect("4"));
        let tail = &bytes[bytes.len() - 8 - len as usize..bytes.len() - 8];
        let footer = RawFooter::parse(tail, bytes.len() as u64).expect("parses");
        let rows = MetadataTable::Columns.rows(&footer, "f.parquet").expect("rows");
        rows.iter()
            .map(|row| {
                assert_eq!(row[6], Value::BigInt(0));
                [row[19].clone(), row[20].clone(), row[21].clone(), row[22].clone()]
            })
            .collect()
    }

    #[test]
    fn dictionary_pages_come_out_the_size_the_pins_do() {
        // COPY (SELECT i % 3 AS a, CASE WHEN i % 4 = 0 THEN NULL ELSE (i % 2)::VARCHAR END AS s,
        // i AS u FROM range(20) r(i)) TO 'd.parquet', in the pin.
        let fields = vec![
            Field::new("a", LogicalType::BigInt),
            Field::new("s", LogicalType::Varchar),
            Field::new("u", LogicalType::BigInt),
        ];
        let (mut a, mut s, mut u) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..20_i64 {
            a.push(Value::BigInt(i % 3));
            s.push(if i % 4 == 0 { Value::Null } else { Value::Varchar((i % 2).to_string()) });
            u.push(Value::BigInt(i));
        }
        let chunk = Chunk::new(vec![
            column(LogicalType::BigInt, &a),
            column(LogicalType::Varchar, &s),
            column(LogicalType::BigInt, &u),
        ])
        .expect("a chunk");
        let mut writer =
            Writer::new(Vec::new(), &fields, Codec::Snappy, "rudb test").expect("starts");
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        let big = |values: [i64; 4]| values.map(Value::BigInt);
        let [first, second, third] = <[_; 3]>::try_from(layout(&bytes)).expect("three");
        assert_eq!(first, big([4, 39, 72, 127]));
        assert_eq!(second, big([76, 101, 68, 112]));
        assert_eq!(third[0], Value::Null);
        assert_eq!(third[1..], big([0, 144, 159, 185])[1..]);
        let chunks = crate::reader::read(Box::new(Memory(bytes))).expect("reads");
        let back: Vec<_> =
            chunks.iter().flat_map(|c| (0..c.len()).map(|row| c.value_at(row, 1))).collect();
        assert_eq!(back, s);
    }

    #[test]
    fn signed_zeros_and_nans_share_an_entry_and_an_enum_lists_every_member() {
        // The pin's COPY of 0.0 and -0.0, of NaN and -NaN, and of ENUM('e0', 'e1', 'e2') that is
        // NULL for the first three rows and then 'e' || (i % 2), over range(20), with
        // WRITE_BLOOM_FILTER false. The doubles go through a nullif that never matches, since the
        // pin writes a column its plan proves has no NULL as REQUIRED.
        let labels: std::sync::Arc<[String]> = ["e0", "e1", "e2"].map(String::from).into();
        let members = LogicalType::Enum(labels);
        let fields = vec![
            Field::new("d", LogicalType::Double),
            Field::new("n", LogicalType::Double),
            Field::new("e", members.clone()),
        ];
        let (mut d, mut n, mut e) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..20 {
            d.push(Value::Double(if i % 2 == 0 { 0.0 } else { -0.0 }));
            n.push(Value::Double(if i % 3 == 0 { f64::NAN } else { -f64::NAN }));
            e.push(if i < 3 { Value::Null } else { Value::Varchar(format!("e{}", i % 2)) });
        }
        let chunk = Chunk::new(vec![
            column(LogicalType::Double, &d),
            column(LogicalType::Double, &n),
            column(members, &e),
        ])
        .expect("a chunk");
        let mut writer =
            Writer::new(Vec::new(), &fields, Codec::Snappy, "rudb test").expect("starts");
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        let big = |values: [i64; 4]| values.map(Value::BigInt);
        let layout = layout(&bytes);
        assert_eq!(
            layout,
            vec![big([4, 27, 51, 47]), big([55, 78, 51, 47]), big([106, 137, 77, 150])]
        );
    }

    #[test]
    fn a_data_page_closes_once_its_values_reach_the_limit() {
        // COPY (SELECT i FROM range(10) r(i)) TO 'p.parquet' (DICTIONARY_SIZE_LIMIT 0,
        // DATA_PAGE_SIZE_LIMIT 16) in the pin writes five pages of two values.
        let fields = vec![Field::new("i", LogicalType::BigInt)];
        let values: Vec<_> = (0..10).map(Value::BigInt).collect();
        let chunk = Chunk::new(vec![column(LogicalType::BigInt, &values)]).expect("a chunk");
        let mut writer =
            Writer::new(Vec::new(), &fields, Codec::Snappy, "rudb test").expect("starts");
        writer.set_limits(super::Limits {
            dictionary: Some(0),
            data_page: 16,
            ..Default::default()
        });
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        let expected = [Value::Null, Value::BigInt(4), Value::BigInt(204), Value::BigInt(195)];
        assert_eq!(layout(&bytes), vec![expected]);
        let chunks = crate::reader::read(Box::new(Memory(bytes))).expect("reads");
        let back: Vec<_> =
            chunks.iter().flat_map(|c| (0..c.len()).map(|row| c.value_at(row, 0))).collect();
        assert_eq!(back, values);
    }

    /// A file held in memory, for reading a written file back.
    #[derive(Debug)]
    struct Memory(Vec<u8>);

    impl rudb_io::File for Memory {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> rudb_common::Result<usize> {
            let start = (offset as usize).min(self.0.len());
            let read = buf.len().min(self.0.len() - start);
            buf[..read].copy_from_slice(&self.0[start..start + read]);
            Ok(read)
        }

        fn write_at(&self, _: u64, _: &[u8]) -> rudb_common::Result<()> {
            Ok(())
        }

        fn sync(&self) -> rudb_common::Result<()> {
            Ok(())
        }

        fn truncate(&self, _: u64) -> rudb_common::Result<()> {
            Ok(())
        }

        fn len(&self) -> rudb_common::Result<u64> {
            Ok(self.0.len() as u64)
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
}
