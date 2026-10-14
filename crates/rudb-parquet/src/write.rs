//! The Parquet writer: a flat schema, dictionary, `PLAIN` or delta values, and `UNCOMPRESSED` or
//! `SNAPPY` pages.
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
//! pin's choice of runs, so a file comes out the same bytes as the pin's for the same rows. A
//! version two file, which `PARQUET_VERSION` asks for, has `RLE_DICTIONARY` pages for a dictionary
//! and the encodings in [`crate::encode`] for a chunk with none.
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

use crate::bloom::{Filter, xxh64};
use crate::encode;
use crate::metadata::Physical;
use crate::runs::{Runs, index_width};
use crate::thrift;

/// How many bytes of a string or a blob a bound keeps, which is the pin's limit.
const BOUND_BYTES: usize = 256;

/// The value encodings this writer uses, by their wire numbers.
const PLAIN: i32 = 0;
const PLAIN_DICTIONARY: i32 = 2;
const DELTA_BINARY_PACKED: i32 = 5;
const DELTA_LENGTH_BYTE_ARRAY: i32 = 6;
const RLE_DICTIONARY: i32 = 8;
const BYTE_STREAM_SPLIT: i32 = 9;

/// How large the pin lets a dictionary and a data page grow, which `COPY ... TO` sets with
/// `DICTIONARY_SIZE_LIMIT`, `STRING_DICTIONARY_PAGE_SIZE_LIMIT` and `DATA_PAGE_SIZE_LIMIT`, and
/// the false positive rate of the bloom filters, which `WRITE_BLOOM_FILTER` and
/// `BLOOM_FILTER_FALSE_POSITIVE_RATIO` set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The most values a dictionary holds, where `None` is a fifth of the rows of the row group
    /// and zero means no dictionary at all.
    pub dictionary: Option<u64>,
    /// The most bytes the values of a string or a blob dictionary take, lengths and all.
    pub string_dictionary: u64,
    /// The estimated size of its values at which a data page is closed.
    pub data_page: u64,
    /// The false positive rate of the bloom filter of each dictionary column, where `None` writes
    /// no bloom filters.
    pub bloom: Option<f64>,
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
            bloom: Some(0.01),
        }
    }
}

/// How many rows the pin hands its column writers at a time, which is where a page under a list
/// is allowed to end.
const BLOCK_ROWS: usize = 2048;

/// One leaf column as it is stored, which for a flat column is the column itself.
#[derive(Debug)]
struct Column {
    /// Its name in the schema.
    name: String,
    /// The names from the top of the schema down to it, which is its `path_in_schema`.
    path: Vec<String>,
    /// The same names without the repeated groups of a list or a map, which are what
    /// `RETURN_STATS` calls it by.
    reported_path: Vec<String>,
    /// The definition level at which it holds a value, and the most its repetition level gets to.
    /// A flat column has one and zero.
    max_def: u16,
    max_rep: u16,
    /// Whether it is `OPTIONAL` rather than `REQUIRED`, which only a map's key is.
    optional: bool,
    /// Whether a page of it can end anywhere, which under a list the pin only lets it do where
    /// a block of [`BLOCK_ROWS`] rows starts.
    spans: bool,
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

/// How a column of the query is laid out: a leaf, which is one column chunk, or a group of the
/// columns under it, with the levels that say how much of it is there.
#[derive(Debug)]
enum Shape {
    /// The leaf at this index of the writer's columns.
    Leaf(usize),
    /// A struct, which is there at definition level `def`.
    Struct { fields: Vec<Shape>, def: u16 },
    /// A list, which is there and empty at `def` and whose elements after the first repeat at
    /// `rep + 1`.
    List { element: Box<Shape>, def: u16, rep: u16 },
    /// An array of `size` elements. The pin writes it as a list, except that a null array still
    /// takes `size` entries.
    Array { element: Box<Shape>, size: usize, def: u16, rep: u16 },
    /// A map, which has the levels of a list and the key and the value of each entry under it.
    Map { key: Box<Shape>, value: Box<Shape>, def: u16, rep: u16 },
}

/// One `SchemaElement` of the footer: a group, or the leaf at an index of the writer's columns.
#[derive(Debug)]
enum Element {
    Group { name: String, repetition: i32, children: usize, converted: Option<i32> },
    Leaf(usize),
}

/// Where a column goes in the schema: under which names, at which levels, and whether it may
/// be null and may have its pages end anywhere.
#[derive(Debug, Clone)]
struct Place {
    path: Vec<String>,
    reported_path: Vec<String>,
    def: u16,
    rep: u16,
    optional: bool,
    spans: bool,
}

/// The leaves and the schema elements of the columns laid out so far.
#[derive(Debug, Default)]
struct Layout {
    columns: Vec<Column>,
    schema: Vec<Element>,
}

impl Layout {
    /// Lays out a column named `name` of the type `ty` the query gave it, the way the pin does: a
    /// struct is a group, a list or an array is a group holding a repeated group named `list`
    /// that holds the `element`, and a map is a group holding a repeated group named `key_value`
    /// that holds a required `key` and a `value`. The definition level of a column is one more
    /// than its group's when it can be null, and a list's element is two more, for an empty
    /// list and a null element.
    fn lay_out(&mut self, name: &str, ty: &LogicalType, at: Place) -> Result<Shape> {
        let Place { mut path, mut reported_path, def, rep, optional, spans } = at;
        path.push(name.to_string());
        reported_path.push(name.to_string());
        let def = if optional { def } else { def - 1 };
        let repetition = i32::from(optional);
        let group = |name: &str, repetition, children, converted| Element::Group {
            name: name.to_string(),
            repetition,
            children,
            converted,
        };
        match ty {
            LogicalType::Struct(fields) => {
                if fields.is_empty() {
                    return Err(Error::invalid_input(
                        "Empty STRUCT columns are not supported in the Parquet format",
                    ));
                }
                self.schema.push(group(name, repetition, fields.len(), None));
                let mut shapes = Vec::with_capacity(fields.len());
                for (at, field) in fields.iter().enumerate() {
                    let place = Place {
                        path: path.clone(),
                        reported_path: reported_path.clone(),
                        def: def + 1,
                        rep,
                        optional: true,
                        spans,
                    };
                    shapes.push(self.lay_out(&member_name(&field.name, at), &field.ty, place)?);
                }
                Ok(Shape::Struct { fields: shapes, def })
            }
            LogicalType::List(element) | LogicalType::Array(element, _) => {
                self.schema.push(group(name, repetition, 1, Some(3)));
                self.schema.push(group("list", 2, 1, None));
                path.push("list".to_string());
                let place = Place {
                    path,
                    reported_path,
                    def: def + 2,
                    rep: rep + 1,
                    optional: true,
                    spans: false,
                };
                let element = Box::new(self.lay_out("element", element, place)?);
                Ok(match ty {
                    LogicalType::Array(_, size) => {
                        Shape::Array { element, size: *size as usize, def, rep }
                    }
                    _ => Shape::List { element, def, rep },
                })
            }
            LogicalType::Map(key, value) => {
                self.schema.push(group(name, repetition, 1, Some(1)));
                self.schema.push(group("key_value", 2, 2, None));
                path.push("key_value".to_string());
                let place = |optional| Place {
                    path: path.clone(),
                    reported_path: reported_path.clone(),
                    def: def + 2,
                    rep: rep + 1,
                    optional,
                    spans: false,
                };
                let key = Box::new(self.lay_out("key", key, place(false))?);
                let value = Box::new(self.lay_out("value", value, place(true))?);
                Ok(Shape::Map { key, value, def, rep })
            }
            _ => {
                let mut column = leaf(name, ty)?;
                column.path = path;
                column.reported_path = reported_path;
                column.max_def = def;
                column.max_rep = rep;
                column.optional = optional;
                column.spans = spans;
                self.schema.push(Element::Leaf(self.columns.len()));
                self.columns.push(column);
                Ok(Shape::Leaf(self.columns.len() - 1))
            }
        }
    }
}

/// The name a struct's member at `at` is written under, which for a member with no name, as the
/// members of a row like `(1, 'a')` are, is the name the pin gives the member of a tuple.
#[must_use]
pub fn member_name(name: &str, at: usize) -> Cow<'_, str> {
    if name.is_empty() { Cow::Owned(format!("element{}", at + 1)) } else { Cow::Borrowed(name) }
}

/// A leaf column of the type `ty` the query gave it, at the levels of a flat column.
fn leaf(name: &str, ty: &LogicalType) -> Result<Column> {
    let stored = storage(ty)?;
    let physical = physical(&stored).ok_or_else(|| {
        Error::not_implemented(format!(
            "COPY TO a Parquet file with a column of type {ty} is not supported yet"
        ))
    })?;
    let bounds = match (ty, &stored) {
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
    let members = match ty {
        LogicalType::Enum(labels) => Some(Members {
            labels: Arc::clone(labels),
            index: labels.iter().zip(0..).map(|(label, at)| (label.clone(), at)).collect(),
            seen: vec![false; labels.len()],
        }),
        _ => None,
    };
    Ok(Column {
        name: name.to_string(),
        path: vec![name.to_string()],
        reported_path: vec![name.to_string()],
        max_def: 1,
        max_rep: 0,
        optional: true,
        spans: true,
        ty: stored,
        physical,
        bounds,
        reported: !matches!(ty, LogicalType::Enum(_)),
        dictionary: !matches!(physical, Physical::Boolean | Physical::FixedLenByteArray),
        members,
    })
}

/// One leaf column of a row group while its values are read.
#[derive(Debug, Default)]
struct Gathering {
    gathered: Gathered,
    written: Written,
    /// The dictionary so far, by the bytes of each value, and its page.
    entries: HashMap<Vec<u8>, u32>,
    page: Vec<u8>,
    /// Whether values still go in the dictionary.
    open: bool,
    /// How many entries say a struct right above the column is null, which the pin counts with
    /// the column's own nulls.
    chain: i64,
    /// How many entries say a group above the column is null or a list above it is empty, which
    /// the pin counts as nulls of a column under a list.
    parent: i64,
}

impl Gathering {
    fn level(&mut self, column: &Column, rep: u16, def: u16) {
        self.gathered.defs.push(def);
        if column.max_rep > 0 {
            self.gathered.reps.push(rep);
        }
    }
}

/// Walks the values of one column of the query down to its leaves.
struct Walk<'a> {
    columns: &'a mut [Column],
    states: &'a mut [Gathering],
    /// The index of the first of `columns` among the writer's.
    first: usize,
    /// The most entries a dictionary takes.
    most: u64,
    limits: Limits,
}

impl Walk<'_> {
    /// Takes `value`, whose first entry repeats at `rep`, into the leaves under `shape`.
    fn value(&mut self, shape: &Shape, value: &Value, rep: u16) -> Result<()> {
        let unexpected = || {
            Error::internal(format!("a {} value in a nested parquet column", value.logical_type()))
        };
        match shape {
            Shape::Leaf(at) => self.leaf(at - self.first, value, rep),
            Shape::Struct { fields, def } => match value {
                Value::Null => {
                    for field in fields {
                        self.absent(field, rep, def - 1, false, true);
                    }
                    Ok(())
                }
                Value::Struct(members) => {
                    for (field, (_, member)) in fields.iter().zip(members) {
                        self.value(field, member, rep)?;
                    }
                    Ok(())
                }
                _ => Err(unexpected()),
            },
            Shape::List { element, def, rep: own }
            | Shape::Array { element, def, rep: own, .. } => match value {
                Value::Null => {
                    if let Shape::Array { size, .. } = shape {
                        for k in 0..*size {
                            self.absent(
                                element,
                                if k == 0 { rep } else { own + 1 },
                                def - 1,
                                false,
                                false,
                            );
                        }
                    } else {
                        self.absent(element, rep, def - 1, true, false);
                    }
                    Ok(())
                }
                Value::List { values, .. } if values.is_empty() => {
                    self.absent(element, rep, *def, true, false);
                    Ok(())
                }
                Value::List { values, .. } => {
                    for (k, value) in values.iter().enumerate() {
                        self.value(element, value, if k == 0 { rep } else { own + 1 })?;
                    }
                    Ok(())
                }
                _ => Err(unexpected()),
            },
            Shape::Map { key, value: held, def, rep: own } => match value {
                Value::Null => {
                    self.absent(key, rep, def - 1, true, false);
                    self.absent(held, rep, def - 1, true, false);
                    Ok(())
                }
                Value::Map { entries, .. } if entries.is_empty() => {
                    self.absent(key, rep, *def, true, false);
                    self.absent(held, rep, *def, true, false);
                    Ok(())
                }
                Value::Map { entries, .. } => {
                    for (k, (entry_key, entry_value)) in entries.iter().enumerate() {
                        let rep = if k == 0 { rep } else { own + 1 };
                        self.value(key, entry_key, rep)?;
                        self.value(held, entry_value, rep)?;
                    }
                    Ok(())
                }
                _ => Err(unexpected()),
            },
        }
    }

    /// Writes an entry at `def` into every leaf under `shape`, for a group above them that is
    /// null or a list above them that is empty. `empty` says it came from a list, under which
    /// an array takes one entry rather than one for each element, and `counted` that it came
    /// from a struct with nothing but structs between it and the leaf.
    fn absent(&mut self, shape: &Shape, rep: u16, def: u16, empty: bool, counted: bool) {
        match shape {
            Shape::Leaf(at) => {
                let at = at - self.first;
                let state = &mut self.states[at];
                state.level(&self.columns[at], rep, def);
                state.parent += 1;
                state.chain += i64::from(counted);
            }
            Shape::Struct { fields, .. } => {
                for field in fields {
                    self.absent(field, rep, def, empty, counted);
                }
            }
            Shape::List { element, .. } => self.absent(element, rep, def, true, false),
            Shape::Map { key, value, .. } => {
                self.absent(key, rep, def, true, false);
                self.absent(value, rep, def, true, false);
            }
            Shape::Array { element, size, rep: own, .. } => {
                if empty {
                    self.absent(element, rep, def, true, false);
                } else {
                    for k in 0..*size {
                        self.absent(element, if k == 0 { rep } else { own + 1 }, def, false, false);
                    }
                }
            }
        }
    }

    /// Takes one value of the leaf at `at`: its levels, its statistics, its plain encoding,
    /// and its place in the dictionary the pin would build, which takes the first of each value
    /// until there would be more entries than a fifth of the rows of the group, or than
    /// `DICTIONARY_SIZE_LIMIT`, or for strings and blobs more bytes than
    /// `STRING_DICTIONARY_PAGE_SIZE_LIMIT`.
    fn leaf(&mut self, at: usize, value: &Value, rep: u16) -> Result<()> {
        let column = &mut self.columns[at];
        let state = &mut self.states[at];
        if value.is_null() {
            if !column.optional {
                return Err(Error::io(
                    "Parquet writer: map key column is not allowed to contain NULL values",
                ));
            }
            state.level(column, rep, column.max_def - 1);
            state.written.nulls += 1;
            return Ok(());
        }
        state.level(column, rep, column.max_def);
        let (gathered, written) = (&mut state.gathered, &mut state.written);
        if is_nan(value) {
            written.nans += 1;
        } else if column.members.is_none() {
            written.take(value, column.bounds);
        }
        let begin = gathered.plain.len();
        plain(value, column, &mut gathered.plain, true)?;
        gathered.ends.push(gathered.plain.len());
        if let Some(members) = &mut column.members {
            let index = match value {
                Value::Varchar(label) => members.index.get(label.as_str()).copied(),
                _ => None,
            };
            let index = index.ok_or_else(|| {
                Error::internal(format!("{value} is not a member of the enum written"))
            })?;
            members.seen[index as usize] = true;
            gathered.indices.push(index);
        } else if state.open {
            let text = column.physical == Physical::ByteArray;
            let bytes = &gathered.plain[begin..];
            let key = dictionary_key(bytes, column.physical);
            if let Some(&index) = state.entries.get(key.as_ref()) {
                gathered.indices.push(index);
            } else if state.entries.len() as u64 + 1 > self.most
                || (text && (state.page.len() + bytes.len()) as u64 > self.limits.string_dictionary)
            {
                state.open = false;
            } else {
                let index = state.entries.len() as u32;
                state.entries.insert(key.into_owned(), index);
                state.page.extend_from_slice(bytes);
                gathered.hashes.push(xxh64(if text { &bytes[4..] } else { bytes }));
                gathered.indices.push(index);
            }
        }
        Ok(())
    }
}

/// What is left to do once every value of a leaf has been read: its dictionary, and its null
/// count, which takes in the nulls of the structs right above it and, under a list, every entry
/// that says a group above it is missing.
fn finish(state: Gathering, column: &Column) -> (Gathered, Written) {
    let Gathering { mut gathered, mut written, entries, page, open, chain, parent } = state;
    written.nulls += chain + if column.max_rep > 0 { parent } else { 0 };
    if let Some(members) = &column.members {
        // The members no row has held are in the dictionary as empty strings, and the bounds
        // are those of the members held so far in the file, which is the pin's.
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
        gathered.hashes = Vec::new();
    }
    (gathered, written)
}

/// One column of a row group, read in full before any of it is written, because whether a
/// dictionary holds it is only known once every value has been seen.
#[derive(Debug, Default)]
struct Gathered {
    /// The definition level of each entry, of which a flat column has one for each row.
    defs: Vec<u16>,
    /// The repetition level of each entry, for a column under a list or a map.
    reps: Vec<u16>,
    /// Where the entries of each block of [`BLOCK_ROWS`] rows start.
    starts: Vec<usize>,
    /// The plain encoding of each value, framed, one after another. A boolean's is one byte.
    plain: Vec<u8>,
    /// Where each value's encoding ends in `plain`.
    ends: Vec<usize>,
    /// The values as dictionary indices, when a dictionary holds them.
    indices: Vec<u32>,
    /// The body of the dictionary page and how many values it holds, when there is one.
    dictionary: Option<(Vec<u8>, usize)>,
    /// The hash of each value of the dictionary, for the bloom filter. An enum's are not kept,
    /// because the pin writes no bloom filter for one.
    hashes: Vec<u64>,
}

/// A row group that has been written but whose metadata waits for the footer, because the bloom
/// filters go after the last row group and each column chunk says where its own filter is.
#[derive(Debug)]
struct Group {
    /// The metadata of each column chunk, still open, and its bloom filter when it has one.
    columns: Vec<(thrift::Writer, Option<Filter>)>,
    total: i64,
    rows: i64,
    start: u64,
    compressed: i64,
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
    /// The leaf columns, in the order of the schema.
    columns: Vec<Column>,
    /// How each column of the query is laid out, and which of the leaves are its own.
    shapes: Vec<Shape>,
    leaves: Vec<Range<usize>>,
    schema: Vec<Element>,
    codec: Codec,
    groups: Vec<Group>,
    rows: i64,
    created_by: String,
    key_values: Vec<(String, Vec<u8>)>,
    totals: Vec<ColumnStatistics>,
    /// For each leaf, whether a row group has had no bounds, which leaves the file with none.
    unbounded: Vec<bool>,
    limits: Limits,
    /// The field id of each element of [`Self::schema`], from [`Writer::set_field_ids`].
    field_ids: Vec<Option<i32>>,
    /// The format version the footer states, from [`Writer::set_version`].
    version: i32,
}

/// The field id `FIELD_IDS` gives a column, with the ones it gives the columns under it.
///
/// The element of a list is under it as `element`, and the key and the value of a map as `key`
/// and `value`, which are the names the pin writes them with. The repeated group between a list or
/// a map and what it holds has no id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldId {
    /// The name of the column, which is matched to a column's without regard to case.
    pub name: String,
    /// The id, where one was given.
    pub id: Option<i32>,
    /// The ids of the columns under it.
    pub children: Vec<FieldId>,
}

/// What one column of a file holds over all of its row groups, which is what a `COPY ... TO`
/// with `RETURN_STATS` reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnStatistics {
    /// The name of the column and of each column it is in, the outermost first, which leaves
    /// out the repeated group of a list or a map.
    pub path: Vec<String>,
    /// How many values it holds, the nulls among them, which under a list counts its levels.
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
/// For a type this writer has no Parquet form for yet, at any depth: a union, `UUID`, `INTERVAL`,
/// `BIT`, `TIME WITH TIME ZONE` and nanosecond timestamps.
pub fn storage(ty: &LogicalType) -> Result<LogicalType> {
    Ok(match ty {
        LogicalType::List(element) => LogicalType::List(Box::new(storage(element)?)),
        LogicalType::Array(element, size) => LogicalType::Array(Box::new(storage(element)?), *size),
        LogicalType::Map(key, value) => {
            LogicalType::Map(Box::new(storage(key)?), Box::new(storage(value)?))
        }
        LogicalType::Struct(fields) => LogicalType::Struct(
            fields
                .iter()
                .map(|field| Ok(Field { ty: storage(&field.ty)?, ..field.clone() }))
                .collect::<Result<_>>()?,
        ),
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
        let mut layout = Layout::default();
        let (mut shapes, mut leaves) = (Vec::new(), Vec::new());
        for field in fields {
            let first = layout.columns.len();
            let top = Place {
                path: Vec::new(),
                reported_path: Vec::new(),
                def: 1,
                rep: 0,
                optional: true,
                spans: true,
            };
            shapes.push(layout.lay_out(&field.name, &field.ty, top)?);
            leaves.push(first..layout.columns.len());
        }
        let Layout { columns, schema } = layout;
        let totals = columns
            .iter()
            .map(|column| ColumnStatistics {
                path: column.reported_path.clone(),
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
            shapes,
            leaves,
            schema,
            codec,
            groups: Vec::new(),
            rows: 0,
            created_by: created_by.to_string(),
            key_values: Vec::new(),
            totals,
            unbounded,
            limits: Limits::default(),
            field_ids: Vec::new(),
            version: 1,
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

    /// Sets the format version, which `PARQUET_VERSION` gives as one or two. A version two file
    /// writes `RLE_DICTIONARY` pages for a dictionary, and a chunk with no dictionary falls back
    /// to `DELTA_BINARY_PACKED` for an integer, `DELTA_LENGTH_BYTE_ARRAY` for a string or a blob
    /// and `BYTE_STREAM_SPLIT` for a float, which is what the pin does. Its data pages are still
    /// version one pages, as the pin's are.
    pub fn set_version(&mut self, version: i32) {
        self.version = version;
    }

    /// Gives the columns that `ids` names their field ids, which go in the footer's schema.
    pub fn set_field_ids(&mut self, ids: &[FieldId]) {
        self.field_ids = vec![None; self.schema.len()];
        let mut at = 0;
        for _ in 0..self.shapes.len() {
            self.mark(ids, &mut at);
        }
    }

    /// Gives the schema element at `at` and the ones under it their ids, where `ids` are the ids
    /// of it and the columns beside it, and moves `at` past them.
    fn mark(&mut self, ids: &[FieldId], at: &mut usize) {
        let (name, mut children, converted) = match &self.schema[*at] {
            Element::Group { name, children, converted, .. } => (name, *children, *converted),
            Element::Leaf(column) => (&self.columns[*column].name, 0, None),
        };
        let found = ids.iter().find(|id| id.name.eq_ignore_ascii_case(name));
        self.field_ids[*at] = found.and_then(|id| id.id);
        *at += 1;
        // A list and a map hold a repeated group with no id, and the ids under the column are for
        // what is under that.
        if converted.is_some()
            && let Element::Group { children: held, .. } = &self.schema[*at]
        {
            children = *held;
            *at += 1;
        }
        let under = found.map_or(&[][..], |id| &id.children);
        for _ in 0..children {
            self.mark(under, at);
        }
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
        if chunks.iter().any(|chunk| chunk.width() != self.shapes.len()) {
            return Err(Error::internal("a chunk with the wrong number of columns"));
        }
        let start = self.at;
        let mut columns = Vec::with_capacity(self.columns.len());
        let (mut total, mut compressed) = (0, 0);
        for field in 0..self.shapes.len() {
            let first = self.leaves[field].start;
            for (column, (gathered, mut written)) in
                (first..).zip(self.gather(field, chunks, rows)?)
            {
                self.write_leaf(column, &gathered, &mut written, &mut columns)?;
                total += written.uncompressed;
                compressed += written.compressed;
            }
        }
        self.groups.push(Group { columns, total, rows: rows as i64, start, compressed });
        self.rows += rows as i64;
        Ok(())
    }

    /// Writes one leaf column of a row group and takes in what it holds.
    fn write_leaf(
        &mut self,
        column: usize,
        gathered: &Gathered,
        written: &mut Written,
        columns: &mut Vec<(thrift::Writer, Option<Filter>)>,
    ) -> Result<()> {
        columns.push(self.write_chunk(column, gathered, written)?);
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
        Ok(())
    }

    /// Reads one column of the query for a row group into its leaves, taking in their statistics
    /// and building the dictionaries the pin would. A leaf that outgrows its dictionary, or has no
    /// value at all, is written plain. A boolean or a sixteen byte decimal never has one, and an
    /// enum always has its own, which lists every member.
    fn gather(
        &mut self,
        field: usize,
        chunks: &[Chunk],
        rows: usize,
    ) -> Result<Vec<(Gathered, Written)>> {
        let limits = self.limits;
        let leaves = self.leaves[field].clone();
        let mut states: Vec<Gathering> = self.columns[leaves.clone()]
            .iter()
            .map(|column| Gathering { open: column.dictionary, ..Gathering::default() })
            .collect();
        let mut walk = Walk {
            columns: &mut self.columns[leaves.clone()],
            states: &mut states,
            first: leaves.start,
            most: limits.dictionary.unwrap_or(rows as u64 / 5),
            limits,
        };
        let mut row = 0;
        for chunk in chunks {
            let vector = chunk.column(field)?;
            // row at a time: the Parquet writer encodes values one by one; a writer that takes a
            // vector's flat data whole is not built yet.
            for at in 0..vector.len() {
                if row % BLOCK_ROWS == 0 {
                    for state in walk.states.iter_mut() {
                        state.gathered.starts.push(state.gathered.defs.len());
                    }
                }
                row += 1;
                walk.value(&self.shapes[field], &vector.try_value_at(at)?, 0)?;
            }
        }
        Ok(states
            .into_iter()
            .zip(&self.columns[leaves])
            .map(|(state, column)| finish(state, column))
            .collect())
    }

    /// Writes one column of a row group, its dictionary page first when it has one, and hands
    /// back its `ColumnChunk`.
    fn write_chunk(
        &mut self,
        at: usize,
        gathered: &Gathered,
        written: &mut Written,
    ) -> Result<(thrift::Writer, Option<Filter>)> {
        let physical = self.columns[at].physical;
        let enumerated = self.columns[at].members.is_some();
        let (encoding, width) = match &gathered.dictionary {
            Some((_, count)) if enumerated || self.version == 2 => {
                (RLE_DICTIONARY, index_width(*count))
            }
            Some((_, count)) => (PLAIN_DICTIONARY, index_width(*count)),
            None => (self.fallback(at), 0),
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
        let column = &self.columns[at];
        let (max_rep, max_def) = (column.max_rep, column.max_def);
        let split = pages(gathered, column, width, self.limits.data_page);
        for (rows, values) in split {
            let mut body = Vec::new();
            if max_rep > 0 {
                levels(&gathered.reps[rows.clone()], max_rep, &mut body);
            }
            if max_def > 0 {
                levels(&gathered.defs[rows.clone()], max_def, &mut body);
            }
            let start = values.start.checked_sub(1).map_or(0, |at| gathered.ends[at]);
            let end = values.end.checked_sub(1).map_or(0, |at| gathered.ends[at]);
            let plain = &gathered.plain[start..end];
            if gathered.dictionary.is_some() {
                body.push(width);
                if !values.is_empty() {
                    let mut indices = Runs::new(width);
                    for index in &gathered.indices[values] {
                        indices.push(*index);
                    }
                    body.extend_from_slice(&indices.finish());
                }
            } else if encoding == DELTA_BINARY_PACKED {
                let unsigned = self.columns[at].ty == LogicalType::UInteger;
                let integers = match physical {
                    Physical::Int32 => plain
                        .chunks_exact(4)
                        .map(|bytes| {
                            let bytes = <[u8; 4]>::try_from(bytes).unwrap_or_default();
                            if unsigned {
                                i64::from(u32::from_le_bytes(bytes))
                            } else {
                                i64::from(i32::from_le_bytes(bytes))
                            }
                        })
                        .collect::<Vec<_>>(),
                    _ => plain
                        .chunks_exact(8)
                        .map(|bytes| i64::from_le_bytes(bytes.try_into().unwrap_or_default()))
                        .collect(),
                };
                encode::binary_packed(&integers, &mut body);
            } else if encoding == DELTA_LENGTH_BYTE_ARRAY {
                let mut strings = Vec::with_capacity(values.len());
                let mut from = start;
                for &to in &gathered.ends[values] {
                    strings.push(&gathered.plain[from + 4..to]);
                    from = to;
                }
                encode::length_byte_array(&strings, &mut body);
            } else if encoding == BYTE_STREAM_SPLIT {
                let width = if physical == Physical::Float { 4 } else { 8 };
                encode::stream_split(plain, width, &mut body);
            } else if physical == Physical::Boolean {
                body.extend_from_slice(&packed(plain));
            } else {
                body.extend_from_slice(plain);
            }
            let mut data = thrift::Writer::default();
            data.i32(1, rows.len() as i32);
            data.i32(2, encoding);
            data.i32(3, 3);
            data.i32(4, 3);
            self.page(0, &body, (5, data), written)?;
            written.pages += 1;
        }
        written.values += gathered.defs.len() as i64;
        let filter = match self.limits.bloom {
            Some(ratio) if !gathered.hashes.is_empty() => {
                let mut filter = Filter::new(gathered.hashes.len(), ratio);
                for hash in &gathered.hashes {
                    filter.insert(*hash);
                }
                Some(filter)
            }
            _ => None,
        };
        Ok((self.chunk_meta(at, first, dictionary, encoding, written), filter))
    }

    /// The encoding of the values of column `at` in a chunk with no dictionary. A version one
    /// file has nothing but `PLAIN`, and a version two file has the encodings the pin turns to,
    /// for the types it writes through a dictionary when it can.
    fn fallback(&self, at: usize) -> i32 {
        let column = &self.columns[at];
        if self.version == 1 || !column.dictionary {
            return PLAIN;
        }
        match column.physical {
            Physical::Int32 | Physical::Int64 => DELTA_BINARY_PACKED,
            Physical::ByteArray => DELTA_LENGTH_BYTE_ARRAY,
            Physical::Float | Physical::Double => BYTE_STREAM_SPLIT,
            _ => PLAIN,
        }
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
        let path: Vec<&str> = column.path.iter().map(String::as_str).collect();
        meta.list_of_strings(3, &path);
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
        meta
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
        let mut groups = Vec::with_capacity(self.groups.len());
        for group in std::mem::take(&mut self.groups) {
            let mut columns = Vec::with_capacity(group.columns.len());
            for (mut meta, filter) in group.columns {
                if let Some(filter) = filter {
                    let bytes = filter.bytes();
                    let mut header = thrift::Writer::default();
                    header.i32(1, i32::try_from(bytes.len()).map_err(|_| too_big())?);
                    for id in 2..=4 {
                        let mut choice = thrift::Writer::default();
                        choice.nested(1, thrift::Writer::default());
                        header.nested(id, choice);
                    }
                    let header = header.stop();
                    meta.int(14, self.at as i64);
                    meta.i32(15, i32::try_from(header.len() + bytes.len()).map_err(|_| too_big())?);
                    self.put(&header)?;
                    self.put(&bytes)?;
                }
                // The pin leaves the chunk's own offset at zero and says where it is in the
                // metadata.
                let mut chunk = thrift::Writer::default();
                chunk.int(2, 0);
                chunk.nested(3, meta);
                columns.push(chunk);
            }
            let mut out = thrift::Writer::default();
            out.list_of_nested(1, columns);
            out.int(2, group.total);
            out.int(3, group.rows);
            out.int(5, group.start as i64);
            out.int(6, group.compressed);
            groups.push(out);
        }
        let mut schema = Vec::with_capacity(self.schema.len() + 1);
        let mut root = thrift::Writer::default();
        root.i32(3, 0);
        root.string(4, "duckdb_schema");
        root.i32(5, self.shapes.len() as i32);
        schema.push(root);
        for (at, element_at) in self.schema.iter().enumerate() {
            let id = self.field_ids.get(at).copied().flatten();
            schema.push(match element_at {
                Element::Leaf(column) => element(&self.columns[*column], id),
                Element::Group { name, repetition, children, converted } => {
                    let mut group = thrift::Writer::default();
                    group.i32(3, *repetition);
                    group.string(4, name);
                    group.i32(5, *children as i32);
                    if let Some(converted) = converted {
                        group.i32(6, *converted);
                    }
                    if let Some(id) = id {
                        group.i32(9, id);
                    }
                    group
                }
            });
        }
        let mut footer = thrift::Writer::default();
        footer.i32(1, self.version);
        footer.list_of_nested(2, schema);
        footer.int(3, self.rows);
        footer.list_of_nested(4, groups);
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

/// The `SchemaElement` of one column, with its field id if it has one.
fn element(column: &Column, id: Option<i32>) -> thrift::Writer {
    let mut element = thrift::Writer::default();
    element.i32(1, physical_wire(column.physical));
    if column.physical == Physical::FixedLenByteArray {
        element.i32(2, 16);
    }
    element.i32(3, i32::from(column.optional));
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
    if let Some(id) = id {
        element.i32(9, id);
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

/// Appends a page's levels of one kind, in the hybrid encoding at the width of the most they get
/// to, after their length in bytes.
fn levels(levels: &[u16], most: u16, out: &mut Vec<u8>) {
    let mut runs = Runs::new((16 - most.leading_zeros()) as u8);
    for level in levels {
        runs.push(u32::from(*level));
    }
    let runs = runs.finish();
    out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    out.extend_from_slice(&runs);
}

/// Where the pin ends the data pages of a column chunk, as the entries and the values each one
/// holds. There is one page unless the estimated size of the values on it reaches `limit`, which
/// closes it after the value that did. Under a list the page only closes on the first entry of a
/// block of [`BLOCK_ROWS`] rows, since the pin keeps the values of the rows it is handed at once
/// on one page. A value is estimated at its plain size, which for a boolean is a byte, or with a
/// dictionary at the whole bytes its index takes. A null or a missing list adds nothing.
fn pages(
    gathered: &Gathered,
    column: &Column,
    width: u8,
    limit: u64,
) -> Vec<(Range<usize>, Range<usize>)> {
    let mut pages = Vec::new();
    let (mut row, mut value, mut size, mut values) = (0, 0, 0_u64, 0_usize);
    let mut starts = gathered.starts.iter().peekable();
    for (at, &def) in gathered.defs.iter().enumerate() {
        while starts.next_if(|&&start| start < at).is_some() {}
        let first = starts.peek() == Some(&&at);
        if def != column.max_def {
            continue;
        }
        size += if width == 0 {
            let start = values.checked_sub(1).map_or(0, |last| gathered.ends[last]);
            (gathered.ends[values] - start) as u64
        } else {
            u64::from(width.div_ceil(8))
        };
        values += 1;
        if size >= limit && (column.spans || first) {
            pages.push((row..at + 1, value..values));
            (row, value, size) = (at + 1, values, 0);
        }
    }
    if row < gathered.defs.len() {
        pages.push((row..gathered.defs.len(), value..values));
    }
    pages
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};
    use rudb_compress::Codec;
    use rudb_vector::{Chunk, Vector};

    use super::{Limits, Writer};
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
        writer.set_limits(Limits { dictionary: Some(0), data_page: 16, ..Default::default() });
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        let expected = [Value::Null, Value::BigInt(4), Value::BigInt(204), Value::BigInt(195)];
        assert_eq!(layout(&bytes), vec![expected]);
        let chunks = crate::reader::read(Box::new(Memory(bytes))).expect("reads");
        let back: Vec<_> =
            chunks.iter().flat_map(|c| (0..c.len()).map(|row| c.value_at(row, 0))).collect();
        assert_eq!(back, values);
    }

    #[test]
    fn bloom_filters_follow_the_last_row_group_with_the_pins_bytes() {
        // COPY (SELECT i % 3 AS a, 'v' || (i % 2) AS s, (i % 4)::DOUBLE AS d, i AS u FROM
        // range(20) r(i)) TO 'b.parquet', in the pin.
        let fields = vec![
            Field::new("a", LogicalType::BigInt),
            Field::new("s", LogicalType::Varchar),
            Field::new("d", LogicalType::Double),
            Field::new("u", LogicalType::BigInt),
        ];
        let (mut a, mut s, mut d, mut u) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in 0..20_i64 {
            a.push(Value::BigInt(i % 3));
            s.push(Value::Varchar(format!("v{}", i % 2)));
            d.push(Value::Double((i % 4) as f64));
            u.push(Value::BigInt(i));
        }
        let chunk = Chunk::new(vec![
            column(LogicalType::BigInt, &a),
            column(LogicalType::Varchar, &s),
            column(LogicalType::Double, &d),
            column(LogicalType::BigInt, &u),
        ])
        .expect("a chunk");
        let mut writer =
            Writer::new(Vec::new(), &fields, Codec::Snappy, "rudb test").expect("starts");
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        let len =
            u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().expect("4"));
        let tail = &bytes[bytes.len() - 8 - len as usize..bytes.len() - 8];
        let footer = RawFooter::parse(tail, bytes.len() as u64).expect("parses");
        let rows = MetadataTable::Columns.rows(&footer, "f.parquet").expect("rows");
        let places: Vec<_> = rows.iter().map(|row| [row[24].clone(), row[25].clone()]).collect();
        let at = |offset| [Value::BigInt(offset), Value::BigInt(47)];
        assert_eq!(places, vec![at(366), at(413), at(460), [Value::Null, Value::Null]]);
        let header = [0x15, 0x40, 0x1C, 0x1C, 0, 0, 0x1C, 0x1C, 0, 0, 0x1C, 0x1C, 0, 0, 0];
        let filters = [
            "0002100800000406002400020000060210004004000000984040002020410000",
            "0020200000003000001080001010000080000004200000800040001004001000",
            "0022004206400004082000440004400318000140000a001000c5000040600010",
        ];
        for (offset, filter) in [366, 413, 460].into_iter().zip(filters) {
            assert_eq!(bytes[offset..offset + 15], header);
            let filter: Vec<u8> = (0..32)
                .map(|at| u8::from_str_radix(&filter[at * 2..at * 2 + 2], 16).expect("hex"))
                .collect();
            assert_eq!(bytes[offset + 15..offset + 47], filter[..]);
        }
        assert_eq!(bytes.len() - 8 - len as usize, 460 + 47);
    }

    const PIN: &str = "DuckDB version v2.0.0-dev84237 (build cc7e7bac7f)";

    /// Writes `columns` as one row group the way the pin would, under its own name.
    fn written(fields: &[Field], columns: Vec<Vector>, codec: Codec, limits: Limits) -> Vec<u8> {
        let chunk = Chunk::new(columns).expect("a chunk");
        let mut writer = Writer::new(Vec::new(), fields, codec, PIN).expect("starts");
        writer.set_limits(limits);
        writer.write_group(&[chunk]).expect("writes");
        let bytes = writer.finish().expect("finishes");
        if let Ok(dir) = std::env::var("RUDB_DUMP") {
            std::fs::write(format!("{dir}/{}.parquet", fields[0].name), &bytes).expect("dumps");
        }
        bytes
    }

    fn list(element: LogicalType, values: Vec<Value>) -> Value {
        Value::List { element, values }
    }

    fn row(members: &[(&str, Value)]) -> Value {
        Value::Struct(members.iter().map(|(name, value)| ((*name).into(), value.clone())).collect())
    }

    #[test]
    fn nested_columns_come_out_as_the_pins_bytes() {
        // The four files of the README's nested section, which the pin wrote uncompressed.
        let int = LogicalType::Integer;
        let text = LogicalType::Varchar;
        let ints = LogicalType::List(Box::new(int.clone()));
        let pairs =
            LogicalType::Struct(vec![Field::new("a", int.clone()), Field::new("b", text.clone())]);
        let map = LogicalType::Map(Box::new(int.clone()), Box::new(text.clone()));
        let lists = LogicalType::List(Box::new(ints.clone()));
        let i = Value::Integer;
        let v = |text: &str| Value::Varchar(text.into());
        let map_of = |entries: Vec<(Value, Value)>| Value::Map {
            key: Box::new(int.clone()),
            value: Box::new(text.clone()),
            entries,
        };
        let files: [(&str, LogicalType, Vec<Value>, &[u8]); 4] = [
            (
                "l",
                ints.clone(),
                vec![
                    list(int.clone(), vec![i(1), i(2), Value::Null]),
                    list(int.clone(), vec![]),
                    Value::Null,
                    list(int.clone(), vec![i(3)]),
                ],
                include_bytes!("../testdata/nested-list.parquet"),
            ),
            (
                "s",
                pairs,
                vec![
                    row(&[("a", i(1)), ("b", v("x"))]),
                    Value::Null,
                    row(&[("a", Value::Null), ("b", v("y"))]),
                ],
                include_bytes!("../testdata/nested-struct.parquet"),
            ),
            (
                "m",
                map.clone(),
                vec![
                    map_of(vec![(i(1), v("a")), (i(2), Value::Null)]),
                    map_of(vec![]),
                    Value::Null,
                ],
                include_bytes!("../testdata/nested-map.parquet"),
            ),
            (
                "ll",
                lists,
                vec![
                    list(
                        ints.clone(),
                        vec![list(int.clone(), vec![i(1)]), Value::Null, list(int.clone(), vec![])],
                    ),
                    Value::Null,
                    list(ints.clone(), vec![list(int.clone(), vec![i(2), i(3)])]),
                ],
                include_bytes!("../testdata/nested-lists.parquet"),
            ),
        ];
        for (name, ty, values, pin) in files {
            let fields = [Field::new(name, ty.clone())];
            let bytes =
                written(&fields, vec![column(ty, &values)], Codec::Uncompressed, Limits::default());
            assert!(bytes == pin, "{name} differs from the pin's file");
            assert_eq!(read_back(pin, 0), values, "{name} reads back");
        }
    }

    /// Column `at` of every row of a file, read back.
    fn read_back(bytes: &[u8], at: usize) -> Vec<Value> {
        let chunks = crate::reader::read(Box::new(Memory(bytes.to_vec()))).expect("reads");
        chunks
            .iter()
            .flat_map(|chunk| (0..chunk.len()).map(|row| chunk.value_at(row, at)))
            .collect()
    }

    #[test]
    fn deeper_nesting_with_dictionaries_and_blooms_comes_out_as_the_pins_bytes() {
        // nested.parquet of the README, 5000 rows of every kind of nesting the pin writes.
        let big = LogicalType::BigInt;
        let text = LogicalType::Varchar;
        let bigs = LogicalType::List(Box::new(big.clone()));
        let pair = LogicalType::Array(Box::new(big.clone()), 2);
        let s =
            LogicalType::Struct(vec![Field::new("a", big.clone()), Field::new("b", text.clone())]);
        let m = LogicalType::Map(Box::new(text.clone()), Box::new(big.clone()));
        let element =
            LogicalType::Struct(vec![Field::new("x", big.clone()), Field::new("y", bigs.clone())]);
        let ls = LogicalType::List(Box::new(element.clone()));
        let r = LogicalType::Struct(vec![Field::new("r", big.clone())]);
        let sl = LogicalType::Struct(vec![
            Field::new("p", bigs.clone()),
            Field::new("q", r),
            Field::new("z", pair.clone()),
        ]);
        let types = [bigs.clone(), s, m, pair, ls, sl];
        let names = ["l", "s", "m", "a", "ls", "sl"];
        let mut values: [Vec<Value>; 6] = Default::default();
        let b = Value::BigInt;
        let or_null = |null: bool, value: Value| if null { Value::Null } else { value };
        for i in 0..5000_i64 {
            values[0].push(if i % 7 == 0 {
                Value::Null
            } else if i % 5 == 0 {
                list(big.clone(), vec![])
            } else {
                list(big.clone(), vec![b(i % 4), or_null(i % 3 == 0, b(i % 6))])
            });
            let a = or_null(i % 4 == 0, b(i % 9));
            values[1].push(or_null(
                i % 11 == 0,
                row(&[("a", a), ("b", Value::Varchar(format!("v{}", i % 3)))]),
            ));
            values[2].push(if i % 13 == 0 {
                Value::Null
            } else {
                let entries = if i % 6 == 0 {
                    vec![]
                } else {
                    vec![(Value::Varchar(format!("k{}", i % 3)), b(i % 5))]
                };
                Value::Map { key: Box::new(text.clone()), value: Box::new(big.clone()), entries }
            });
            values[3].push(or_null(i % 9 == 0, list(big.clone(), vec![b(i % 2), b(i % 3)])));
            let y = or_null(i % 2 == 0, list(big.clone(), vec![b(i % 5)]));
            let pairs = list(element.clone(), vec![row(&[("x", b(i % 3)), ("y", y)]), Value::Null]);
            values[4].push(or_null(i % 17 == 0, pairs));
            let q = or_null(i % 8 == 0, row(&[("r", b(i % 7))]));
            let z = list(big.clone(), vec![b(i % 3), b(1)]);
            values[5].push(or_null(
                i % 10 == 0,
                row(&[("p", list(big.clone(), vec![b(i % 2)])), ("q", q), ("z", z)]),
            ));
        }
        let fields: Vec<_> =
            names.iter().zip(&types).map(|(name, ty)| Field::new(*name, ty.clone())).collect();
        let columns =
            types.iter().zip(&values).map(|(ty, values)| column(ty.clone(), values)).collect();
        let bytes = written(&fields, columns, Codec::Snappy, Limits::default());
        let pin = include_bytes!("../testdata/nested.parquet");
        assert!(bytes == pin, "differs from the pin's file");
        for (at, values) in values.iter().enumerate() {
            assert!(read_back(pin, at) == *values, "{} reads back", names[at]);
        }
    }

    /// The columns of a file, their values and the file the pin wrote of them.
    type File<'a> = (Vec<(&'a str, LogicalType)>, Vec<Vec<Value>>, &'a [u8]);

    #[test]
    fn version_two_files_come_out_as_the_pins_bytes() {
        // COPY (SELECT i*7-50 AS a, CASE WHEN i%5=0 THEN NULL ELSE i*i*1000000000 END AS b,
        // (4000000000 - i*3)::UINTEGER AS c, CASE WHEN i%6=0 THEN NULL ELSE repeat('x', i) END
        // AS d, i/3.0 AS e, (i*1.5)::FLOAT AS f, i%2=0 AS g, (i*1.25)::DECIMAL(30,2) AS h,
        // (i%3)::TINYINT AS k, NULL::INTEGER AS n, (200+i)::UTINYINT AS u, TIMESTAMP
        // '2020-01-01' + INTERVAL (i) SECOND AS t, ('b' || i)::BLOB AS bl, (i*1000)::HUGEINT AS
        // hu FROM range(20) r(i)) TO 'v2-types.parquet' (FORMAT parquet, PARQUET_VERSION v2,
        // COMPRESSION uncompressed), in the pin.
        let types = [
            ("a", LogicalType::BigInt),
            ("b", LogicalType::BigInt),
            ("c", LogicalType::UInteger),
            ("d", LogicalType::Varchar),
            ("e", LogicalType::Double),
            ("f", LogicalType::Float),
            ("g", LogicalType::Boolean),
            ("h", LogicalType::Decimal { width: 30, scale: 2 }),
            ("k", LogicalType::TinyInt),
            ("n", LogicalType::Integer),
            ("u", LogicalType::UTinyInt),
            ("t", LogicalType::Timestamp),
            ("bl", LogicalType::Blob),
            ("hu", LogicalType::HugeInt),
        ];
        let mut rows: Vec<Vec<Value>> = vec![Vec::new(); types.len()];
        for i in 0..20_i64 {
            let row = [
                Value::BigInt(i * 7 - 50),
                if i % 5 == 0 { Value::Null } else { Value::BigInt(i * i * 1_000_000_000) },
                Value::UInteger(4_000_000_000 - i as u32 * 3),
                if i % 6 == 0 { Value::Null } else { Value::Varchar("x".repeat(i as usize)) },
                Value::Double(i as f64 / 3.0),
                Value::Float(i as f32 * 1.5),
                Value::Boolean(i % 2 == 0),
                Value::Decimal { unscaled: i128::from(i) * 125, width: 30, scale: 2 },
                Value::TinyInt((i % 3) as i8),
                Value::Null,
                Value::UTinyInt(200 + i as u8),
                Value::Timestamp(1_577_836_800_000_000 + i * 1_000_000),
                Value::Blob(format!("b{i}").into_bytes()),
                // A `HUGEINT` comes to the writer as the `DOUBLE` it is stored as.
                Value::Double((i * 1000) as f64),
            ];
            for (column, value) in rows.iter_mut().zip(row) {
                column.push(value);
            }
        }
        // The same for (SELECT CASE WHEN i%7=0 THEN NULL ELSE (i*i) % 1009 - 500 END AS a FROM
        // range(2100) r(i)), which takes two blocks of deltas.
        let blocks = (0..2100_i64)
            .map(|i| if i % 7 == 0 { Value::Null } else { Value::BigInt((i * i) % 1009 - 500) })
            .collect::<Vec<_>>();
        let files: [File<'_>; 2] = [
            (types.to_vec(), rows, include_bytes!("../testdata/v2-types.parquet")),
            (
                vec![("a", LogicalType::BigInt)],
                vec![blocks],
                include_bytes!("../testdata/v2-blocks.parquet"),
            ),
        ];
        for (types, rows, pin) in files {
            let fields: Vec<_> =
                types.iter().map(|(name, ty)| Field::new(*name, ty.clone())).collect();
            let columns = types
                .iter()
                .zip(&rows)
                .map(|((_, ty), values)| column(super::storage(ty).expect("stored"), values));
            let chunk = Chunk::new(columns.collect()).expect("a chunk");
            let mut writer =
                Writer::new(Vec::new(), &fields, Codec::Uncompressed, PIN).expect("starts");
            writer.set_version(2);
            writer.write_group(&[chunk]).expect("writes");
            let bytes = writer.finish().expect("finishes");
            // The pin writes no nan_count, which a newer DuckDB does and this writer does too, so
            // only what comes before the footer is the pin's byte for byte.
            let footer =
                u32::from_le_bytes(pin[pin.len() - 8..pin.len() - 4].try_into().expect("four"));
            let pages = pin.len() - 8 - footer as usize;
            assert!(bytes[..pages] == pin[..pages], "{} differs from the pin's file", types[0].0);
            for (at, values) in rows.iter().enumerate() {
                assert_eq!(&read_back(pin, at), values, "{} reads back", types[at].0);
            }
        }
    }

    #[test]
    fn a_page_under_a_list_only_closes_where_a_block_of_rows_starts() {
        // list-paged.parquet of the README, with DATA_PAGE_SIZE_LIMIT 1000.
        let big = LogicalType::BigInt;
        let bigs = LogicalType::List(Box::new(big.clone()));
        let (mut l, mut f) = (Vec::new(), Vec::new());
        for i in 0..5000_i64 {
            l.push(if i % 7 == 0 {
                Value::Null
            } else if i % 5 == 0 {
                list(big.clone(), vec![])
            } else {
                let second = if i % 3 == 0 { Value::Null } else { Value::BigInt(i % 6) };
                list(big.clone(), vec![Value::BigInt(i % 4), second])
            });
            f.push(Value::BigInt(i));
        }
        let fields = [Field::new("l", bigs.clone()), Field::new("f", big.clone())];
        let limits = Limits { data_page: 1000, ..Default::default() };
        let bytes =
            written(&fields, vec![column(bigs, &l), column(big, &f)], Codec::Snappy, limits);
        let pin = include_bytes!("../testdata/list-paged.parquet");
        assert!(bytes == pin, "differs from the pin's file");
        assert!(read_back(pin, 0) == l, "the list reads back");
        assert!(read_back(pin, 1) == f, "the column after it reads back");
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
