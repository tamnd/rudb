//! The whole footer as it was written, which is what `parquet_metadata`, `parquet_schema`,
//! `parquet_kv_metadata` and `parquet_file_metadata` print.
//!
//! [`crate::Metadata`] reads the footer a scan needs, narrowed to the fields a reader uses and
//! refusing a nested schema. These four functions print the footer instead, so they need every
//! field, whether each one was written at all, since an absent field is a NULL and a field written
//! as zero is a zero, and a nested schema is one more thing to print. So the footer is read here
//! into a tree of Thrift values with nothing dropped, and each function's rows are read off the
//! tree by field id the way the pin reads them off its generated structures.
//!
//! What the pin prints is the Thrift library's own text for an enum or a structure, such as
//! `BYTE_ARRAY` or `DecimalType(scale=3, precision=18)`, and its own conversion of a statistic into
//! the column's type and then into text. Both are copied here rather than approximated, from a
//! DuckDB newer than the pin where the two differ, which is the `stats_nan_count` column the
//! corpus expects at the end of `parquet_metadata`.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_io::File;

use crate::bloom::{Filter, Stored, probe_hashes};
use crate::metadata::Unit;
use crate::thrift::{Kind, Reader};

/// How deep a footer may nest before it is refused, which no real footer comes near. A file that
/// nests deeper is corrupt or hostile, and either one should be an error and not a stack overflow.
const MAX_DEPTH: usize = 64;

/// One Thrift value as it came off the wire, with a structure kept as its fields in file order.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Bool(bool),
    Int(i64),
    Double(f64),
    Bytes(Vec<u8>),
    List(Vec<Node>),
    Map(Vec<(Node, Node)>),
    Struct(Vec<(i16, Node)>),
}

/// The empty structure an absent one reads as, so a missing field of it reads as absent too.
static EMPTY: Node = Node::Struct(Vec::new());

impl Node {
    /// The field with this id, the last one when a writer repeated it, which is what the pin keeps.
    fn get(&self, id: i16) -> Option<&Node> {
        match self {
            Self::Struct(fields) => {
                fields.iter().rev().find(|(field, _)| *field == id).map(|(_, node)| node)
            }
            _ => None,
        }
    }

    /// The structure in this field, or an empty one when it is absent.
    fn child(&self, id: i16) -> &Node {
        self.get(id).unwrap_or(&EMPTY)
    }

    fn int(&self, id: i16) -> Option<i64> {
        match self.get(id) {
            Some(Self::Int(value)) => Some(*value),
            _ => None,
        }
    }

    fn flag(&self, id: i16) -> Option<bool> {
        match self.get(id) {
            Some(Self::Bool(value)) => Some(*value),
            _ => None,
        }
    }

    fn double(&self, id: i16) -> Option<f64> {
        match self.get(id) {
            Some(Self::Double(value)) => Some(*value),
            _ => None,
        }
    }

    fn bytes(&self, id: i16) -> Option<&[u8]> {
        match self.get(id) {
            Some(Self::Bytes(value)) => Some(value),
            _ => None,
        }
    }

    /// A string field as text, with any bytes that are not UTF-8 replaced.
    fn text(&self, id: i16) -> Option<String> {
        self.bytes(id).map(|bytes| String::from_utf8_lossy(bytes).into_owned())
    }

    /// The elements of a list field, and none when it is absent.
    fn items(&self, id: i16) -> &[Node] {
        match self.get(id) {
            Some(Self::List(items)) => items,
            _ => &[],
        }
    }

    /// The value of an element of a list of integers, which is how a list of enums is written.
    fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }
}

/// Reads one value of the given type, a list or a map element when `in_list` is set, which only
/// matters to a boolean.
fn read_node(reader: &mut Reader<'_>, kind: Kind, in_list: bool, depth: usize) -> Result<Node> {
    if depth > MAX_DEPTH {
        return Err(Error::io("a parquet footer nested too deeply to read"));
    }
    Ok(match kind {
        Kind::BooleanTrue | Kind::BooleanFalse => Node::Bool(reader.read_bool(kind, in_list)?),
        Kind::Byte => Node::Int(i64::from(reader.read_byte()?)),
        Kind::I16 | Kind::I32 | Kind::I64 => Node::Int(reader.read_int()?),
        Kind::Double => Node::Double(reader.read_double()?),
        Kind::Binary => Node::Bytes(reader.read_binary()?.to_vec()),
        Kind::List | Kind::Set => {
            let (len, element) = reader.list_begin()?;
            let mut items = Vec::new();
            for _ in 0..len {
                items.push(read_node(reader, element, true, depth + 1)?);
            }
            Node::List(items)
        }
        Kind::Map => {
            let (len, key, value) = reader.map_begin()?;
            let mut pairs = Vec::new();
            for _ in 0..len {
                let key = read_node(reader, key, true, depth + 1)?;
                pairs.push((key, read_node(reader, value, true, depth + 1)?));
            }
            Node::Map(pairs)
        }
        Kind::Struct => {
            let saved = reader.struct_begin();
            let mut fields = Vec::new();
            while let Some(field) = reader.field_begin()? {
                fields.push((field.id, read_node(reader, field.kind, false, depth + 1)?));
            }
            reader.struct_end(saved);
            Node::Struct(fields)
        }
    })
}

/// A Parquet file's footer with every field it was written with, and the two sizes
/// `parquet_file_metadata` reports.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFooter {
    root: Node,
    file_size: u64,
    footer_size: u64,
}

impl RawFooter {
    /// Reads the footer of an open file, named `path` in what goes wrong.
    ///
    /// # Errors
    ///
    /// In the pin's words when the file is too short, does not end in `PAR1` or states a footer
    /// that does not fit, and when the footer is not a Thrift structure.
    pub fn read(file: &dyn File, path: &str) -> Result<Self> {
        let size = file.len()?;
        if size < 12 {
            return Err(Error::invalid_input(format!(
                "File '{path}' too small to be a Parquet file"
            )));
        }
        let mut tail = [0_u8; 8];
        file.read_exact_at(size - 8, &mut tail)?;
        if &tail[4..] != b"PAR1" {
            return Err(Error::invalid_input(format!(
                "No magic bytes found at end of file '{path}'"
            )));
        }
        let footer = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
        if footer == 0 || size < 12 + footer {
            return Err(Error::invalid_input(format!("Footer length error in file '{path}'")));
        }
        let length = usize::try_from(footer).map_err(|_| Error::io("a footer too long to read"))?;
        let mut bytes = vec![0_u8; length];
        file.read_exact_at(size - 8 - footer, &mut bytes)?;
        Self::parse(&bytes, size)
    }

    /// Parses footer bytes taken from a file of `file_size` bytes.
    ///
    /// # Errors
    ///
    /// When the bytes are not a Thrift structure.
    pub fn parse(bytes: &[u8], file_size: u64) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let root = read_node(&mut reader, Kind::Struct, false, 0)?;
        let footer_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Ok(Self { root, file_size, footer_size })
    }

    fn schema(&self) -> &[Node] {
        self.root.items(2)
    }

    /// The top level column `parquet_bloom_probe` names, by its place among the top level columns,
    /// the type the pin reads it as, which is what the probed value is cast to, and whether it is
    /// nested. The name is matched ignoring case, as the pin's identifiers are, and the first match
    /// wins. A nested column has no single filter to probe, so nothing is ruled out in it, but the
    /// value is still cast to its type.
    ///
    /// # Errors
    ///
    /// When the column's type is one the pin refuses to read.
    pub fn probe_column(&self, name: &str) -> Result<Option<(usize, LogicalType, bool)>> {
        let schema = self.schema();
        let mut start = 1;
        for (at, (_, element)) in self.top_level().enumerate() {
            let (ty, next) = subtree(schema, start)?;
            start = next;
            if element.text(4).is_some_and(|named| named.eq_ignore_ascii_case(name)) {
                return Ok(Some((at, ty, element.int(5).is_some_and(|children| children > 0))));
            }
        }
        Ok(None)
    }

    /// Whether each row group's bloom filter for top level column `column` rules `value` out, the
    /// value already cast to the type [`RawFooter::probe_column`] gave.
    ///
    /// A row group whose chunk has no filter, or a filter that is not one the pin reads, rules
    /// nothing out, and neither does a value the pin does not hash. The filter is the one of the
    /// column's own leaf, where the pin takes the chunk at the column's place among the top level
    /// columns and so reads another column's filter once a nested one comes first, which is
    /// tamnd/duckdb#51.
    ///
    /// # Errors
    ///
    /// When the file cannot be read.
    pub fn bloom_excludes(
        &self,
        file: &dyn File,
        column: usize,
        value: &Value,
    ) -> Result<Vec<bool>> {
        let groups = self.root.items(4);
        let hashes = self.top_level().nth(column).and_then(|(leaf, element)| {
            if element.int(5).is_some_and(|children| children > 0) {
                return None;
            }
            let unit = match derive(element).ok()?.1 {
                Info::Millis => Some(Unit::Millis),
                Info::Micros => Some(Unit::Micros),
                Info::Nanos => Some(Unit::Nanos),
                _ => None,
            };
            let physical = element.int(1).unwrap_or(-1);
            let stored = Stored { physical, width: element.int(2).unwrap_or(0), unit };
            Some((leaf, probe_hashes(value, stored)?))
        });
        let Some((leaf, hashes)) = hashes else {
            return Ok(vec![false; groups.len()]);
        };
        let size = file.len()?;
        let mut excludes = Vec::with_capacity(groups.len());
        for group in groups {
            let filter = match group.items(1).get(leaf) {
                Some(chunk) => read_filter(file, size, chunk.child(3))?,
                None => None,
            };
            let ruled_out =
                filter.is_some_and(|filter| hashes.iter().all(|&hash| !filter.contains(hash)));
            excludes.push(ruled_out);
        }
        Ok(excludes)
    }

    /// The top level columns, each with the first leaf it holds and its own schema element.
    fn top_level(&self) -> impl Iterator<Item = (usize, &Node)> {
        let schema = self.schema();
        let mut at = 1;
        let mut leaf = 0;
        std::iter::from_fn(move || {
            let element = schema.get(at)?;
            let first = leaf;
            // Walks the column's subtree, counting what is still to be seen under it.
            let mut pending = 1_i64;
            while pending > 0 && at < schema.len() {
                let children = schema[at].int(5).unwrap_or(0).max(0);
                if children == 0 {
                    leaf += 1;
                }
                pending += children - 1;
                at += 1;
            }
            Some((first, element))
        })
    }
}

/// The type of the schema element at `at` with everything under it, and where the element after
/// them is. A group is a list when it says it is one, with its element under the repeated group in
/// between, a map when it says it is one, with its key and value under the repeated `key_value`
/// group, and a struct of its children otherwise.
fn subtree(schema: &[Node], at: usize) -> Result<(LogicalType, usize)> {
    let element = schema.get(at).ok_or_else(|| Error::io("a parquet schema cut short"))?;
    let children = element.int(5).unwrap_or(0).max(0);
    if children == 0 {
        return Ok((derive(element)?.0, at + 1));
    }
    let logical = element.get(10);
    let converted = element.int(6);
    let list = converted == Some(3) || logical.is_some_and(|logical| logical.get(3).is_some());
    let map =
        matches!(converted, Some(1 | 2)) || logical.is_some_and(|logical| logical.get(2).is_some());
    if (list || map) && children == 1 {
        let repeated = schema.get(at + 1).ok_or_else(|| Error::io("a parquet schema cut short"))?;
        let inner = repeated.int(5).unwrap_or(0).max(0);
        if map && inner == 2 {
            let (key, next) = subtree(schema, at + 2)?;
            let (value, next) = subtree(schema, next)?;
            return Ok((LogicalType::Map(Box::new(key), Box::new(value)), next));
        }
        if list && inner == 1 {
            let (element, next) = subtree(schema, at + 2)?;
            return Ok((LogicalType::List(Box::new(element)), next));
        }
        let (element, next) = subtree(schema, at + 1)?;
        return Ok((LogicalType::List(Box::new(element)), next));
    }
    let mut fields = Vec::new();
    let mut next = at + 1;
    for _ in 0..children {
        let name = schema.get(next).and_then(|child| child.text(4)).unwrap_or_default();
        let (ty, after) = subtree(schema, next)?;
        fields.push(Field::new(name, ty));
        next = after;
    }
    Ok((LogicalType::Struct(fields), next))
}

/// How far into a filter's header is read when the chunk does not say how long the filter is,
/// which is far more than the four fields of a header take.
const HEADER_MOST: u64 = 256;

/// The bloom filter a column chunk's metadata points at, the pin's checks included: `None` when
/// there is none, when its offset or length is out of the file, when the header is not a block
/// filter of XXH64 left uncompressed, or when its size is not whole blocks or disagrees with the
/// length the chunk states.
fn read_filter(file: &dyn File, size: u64, meta: &Node) -> Result<Option<Filter>> {
    let Some(start) = meta.int(14).and_then(|at| u64::try_from(at).ok()) else {
        return Ok(None);
    };
    if start == 0 || start >= size {
        return Ok(None);
    }
    let length = match meta.int(15) {
        None => None,
        Some(length) => match u64::try_from(length) {
            Ok(length) if length > 0 && length <= size - start => Some(length),
            _ => return Ok(None),
        },
    };
    let window = length.unwrap_or(HEADER_MOST).min(size - start);
    let mut bytes = vec![
        0_u8;
        usize::try_from(window)
            .map_err(|_| Error::io("a bloom filter too long to read"))?
    ];
    file.read_exact_at(start, &mut bytes)?;
    let mut reader = Reader::new(&bytes);
    let Ok(header) = read_node(&mut reader, Kind::Struct, false, 0) else {
        return Ok(None);
    };
    let known = header.child(2).get(1).is_some()
        && header.child(3).get(1).is_some()
        && header.child(4).get(1).is_some();
    let Some(data) = header.int(1).and_then(|data| u64::try_from(data).ok()) else {
        return Ok(None);
    };
    let head = reader.position() as u64;
    if !known || data == 0 || data % 32 != 0 || data > size - start - head {
        return Ok(None);
    }
    if length.is_some_and(|length| head > length || data != length - head) {
        return Ok(None);
    }
    let from = usize::try_from(head).unwrap_or(usize::MAX);
    let to = usize::try_from(head + data).unwrap_or(usize::MAX);
    if let Some(held) = bytes.get(from..to) {
        return Ok(Some(Filter::from_bytes(held)));
    }
    let mut held = vec![0_u8; to - from];
    file.read_exact_at(start + head, &mut held)?;
    Ok(Some(Filter::from_bytes(&held)))
}

/// The four tables that print a footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataTable {
    /// `parquet_metadata`, a row per column chunk of each row group.
    Columns,
    /// `parquet_schema`, a row per schema element, groups included.
    Schema,
    /// `parquet_kv_metadata`, a row per key and value the writer kept.
    KeyValue,
    /// `parquet_file_metadata`, one row for the file.
    File,
}

impl MetadataTable {
    /// The columns the table answers, the pin's names and types in the pin's order.
    #[must_use]
    pub fn fields(self) -> Vec<Field> {
        use LogicalType as T;
        let named = |columns: &[(&str, LogicalType)]| {
            columns.iter().map(|(name, ty)| Field::new(*name, ty.clone())).collect()
        };
        match self {
            Self::Columns => {
                let bounds = ["xmin", "xmax", "ymin", "ymax", "zmin", "zmax", "mmin", "mmax"];
                let bbox = bounds.iter().map(|name| Field::new(*name, T::Double)).collect();
                named(&[
                    ("file_name", T::Varchar),
                    ("row_group_id", T::BigInt),
                    ("row_group_num_rows", T::BigInt),
                    ("row_group_num_columns", T::BigInt),
                    ("row_group_bytes", T::BigInt),
                    ("column_id", T::BigInt),
                    ("file_offset", T::BigInt),
                    ("num_values", T::BigInt),
                    ("path_in_schema", T::Varchar),
                    ("type", T::Varchar),
                    ("stats_min", T::Varchar),
                    ("stats_max", T::Varchar),
                    ("stats_null_count", T::BigInt),
                    ("stats_distinct_count", T::BigInt),
                    ("stats_min_value", T::Varchar),
                    ("stats_max_value", T::Varchar),
                    ("compression", T::Varchar),
                    ("encodings", T::Varchar),
                    ("index_page_offset", T::BigInt),
                    ("dictionary_page_offset", T::BigInt),
                    ("data_page_offset", T::BigInt),
                    ("total_compressed_size", T::BigInt),
                    ("total_uncompressed_size", T::BigInt),
                    ("key_value_metadata", T::map(T::Blob, T::Blob)),
                    ("bloom_filter_offset", T::BigInt),
                    ("bloom_filter_length", T::BigInt),
                    ("min_is_exact", T::Boolean),
                    ("max_is_exact", T::Boolean),
                    ("row_group_compressed_bytes", T::BigInt),
                    ("geo_bbox", T::Struct(bbox)),
                    ("geo_types", T::list(T::Varchar)),
                    ("encoding_stats", T::list(encoding_stat())),
                    ("stats_nan_count", T::BigInt),
                ])
            }
            Self::Schema => named(&[
                ("file_name", T::Varchar),
                ("name", T::Varchar),
                ("type", T::Varchar),
                ("type_length", T::Varchar),
                ("repetition_type", T::Varchar),
                ("num_children", T::BigInt),
                ("converted_type", T::Varchar),
                ("scale", T::BigInt),
                ("precision", T::BigInt),
                ("field_id", T::BigInt),
                ("logical_type", T::Varchar),
                ("duckdb_type", T::Varchar),
                ("column_id", T::BigInt),
            ]),
            Self::KeyValue => {
                named(&[("file_name", T::Varchar), ("key", T::Blob), ("value", T::Blob)])
            }
            Self::File => named(&[
                ("file_name", T::Varchar),
                ("created_by", T::Varchar),
                ("num_rows", T::BigInt),
                ("num_row_groups", T::BigInt),
                ("format_version", T::BigInt),
                ("encryption_algorithm", T::Varchar),
                ("footer_signing_key_metadata", T::Varchar),
                ("file_size_bytes", T::UBigInt),
                ("footer_size", T::UBigInt),
                ("column_orders", T::list(T::Varchar)),
            ]),
        }
    }

    /// The table's rows for one file, which `path` names.
    ///
    /// A value in a `VARCHAR` column can come back as some other type, a statistic in the type of
    /// its column or a type length as an integer, and the caller casts it to text. The pin does
    /// the same, and what a timestamp with a time zone looks like as text is the session's to say.
    ///
    /// # Errors
    ///
    /// When a column's type cannot be worked out from the schema, or a statistic is not the size
    /// its type has, in the pin's words.
    pub fn rows(self, footer: &RawFooter, path: &str) -> Result<Vec<Vec<Value>>> {
        match self {
            Self::Columns => column_rows(footer, path),
            Self::Schema => schema_rows(footer, path),
            Self::KeyValue => Ok(footer
                .root
                .items(5)
                .iter()
                .map(|pair| {
                    let key = pair.bytes(1).unwrap_or_default().to_vec();
                    let value = pair.bytes(2).unwrap_or_default().to_vec();
                    vec![Value::Varchar(path.to_string()), Value::Blob(key), Value::Blob(value)]
                })
                .collect()),
            Self::File => Ok(vec![file_row(footer, path)]),
        }
    }
}

/// The type of an element of `encoding_stats`.
fn encoding_stat() -> LogicalType {
    LogicalType::Struct(vec![
        Field::new("page_type", LogicalType::Varchar),
        Field::new("encoding", LogicalType::Varchar),
        Field::new("count", LogicalType::Integer),
    ])
}

fn big(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::BigInt)
}

fn boolean(value: Option<bool>) -> Value {
    value.map_or(Value::Null, Value::Boolean)
}

fn varchar(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Varchar)
}

fn count(len: usize) -> Value {
    Value::BigInt(i64::try_from(len).unwrap_or(i64::MAX))
}

/// `parquet_metadata`: for each row group, a row for each leaf of the schema.
fn column_rows(footer: &RawFooter, path: &str) -> Result<Vec<Vec<Value>>> {
    // A leaf is an element with no children, which is the pin's test, so an empty group counts.
    let leaves = footer
        .schema()
        .iter()
        .filter(|element| element.int(5).unwrap_or(0) <= 0)
        .map(derive)
        .collect::<Result<Vec<_>>>()?;
    let mut rows = Vec::new();
    for (group_id, group) in footer.root.items(4).iter().enumerate() {
        let chunks = group.items(1);
        for (column_id, (ty, info)) in leaves.iter().enumerate() {
            let chunk = chunks.get(column_id).ok_or_else(|| {
                Error::io(format!(
                    "row group {group_id} of '{path}' has {} columns where the schema has {}",
                    chunks.len(),
                    leaves.len()
                ))
            })?;
            let meta = chunk.child(3);
            let stats = meta.child(12);
            let stat = |id: i16| convert(ty, *info, stats.bytes(id));
            let names = |id: i16, name: fn(i64) -> String| {
                let items = meta.items(id).iter().filter_map(Node::as_int);
                items.map(name).collect::<Vec<_>>().join(", ")
            };
            let path_in_schema = meta
                .items(3)
                .iter()
                .map(|part| match part {
                    Node::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let pairs = meta
                .items(8)
                .iter()
                .map(|pair| {
                    let key = pair.bytes(1).unwrap_or_default().to_vec();
                    (Value::Blob(key), Value::Blob(pair.bytes(2).unwrap_or_default().to_vec()))
                })
                .collect();
            let geo = meta.get(17).unwrap_or(&EMPTY);
            rows.push(vec![
                Value::Varchar(path.to_string()),
                count(group_id),
                Value::BigInt(group.int(3).unwrap_or(0)),
                count(chunks.len()),
                Value::BigInt(group.int(2).unwrap_or(0)),
                count(column_id),
                // The chunk's offset, there when the row group's is, which is the pin's test.
                if group.get(5).is_some() {
                    Value::BigInt(chunk.int(2).unwrap_or(0))
                } else {
                    Value::Null
                },
                Value::BigInt(meta.int(5).unwrap_or(0)),
                Value::Varchar(path_in_schema),
                Value::Varchar(physical_name(meta.int(1).unwrap_or(0))),
                stat(2)?,
                stat(1)?,
                big(stats.int(3)),
                big(stats.int(4)),
                stat(6)?,
                stat(5)?,
                Value::Varchar(codec_name(meta.int(4).unwrap_or(0))),
                Value::Varchar(names(2, encoding_name)),
                big(meta.int(10)),
                big(meta.int(11)),
                Value::BigInt(meta.int(9).unwrap_or(0)),
                Value::BigInt(meta.int(7).unwrap_or(0)),
                Value::BigInt(meta.int(6).unwrap_or(0)),
                Value::Map {
                    key: Box::new(LogicalType::Blob),
                    value: Box::new(LogicalType::Blob),
                    entries: pairs,
                },
                big(meta.int(14)),
                big(meta.int(15)),
                boolean(stats.flag(8)),
                boolean(stats.flag(7)),
                big(group.int(6)),
                bbox(geo.get(1)),
                geo_types(geo)?,
                encoding_stats(meta),
                big(stats.int(9)),
            ]);
        }
    }
    Ok(rows)
}

/// The bounding box of a column's geospatial statistics, and NULL when it has none.
fn bbox(node: Option<&Node>) -> Value {
    let Some(node) = node else {
        return Value::Null;
    };
    let names = ["xmin", "xmax", "ymin", "ymax", "zmin", "zmax", "mmin", "mmax"];
    let fields = (1_i16..)
        .zip(names)
        .map(|(id, name)| {
            // The x and y bounds are required, so an absent one is the zero the pin defaults to.
            let value = node.double(id).or((id <= 4).then_some(0.0));
            (name.to_string(), value.map_or(Value::Null, Value::Double))
        })
        .collect();
    Value::Struct(fields)
}

/// The geometry types of a column's geospatial statistics, which nothing here prints yet.
fn geo_types(geo: &Node) -> Result<Value> {
    if geo.get(2).is_none() {
        return Ok(Value::Null);
    }
    Err(Error::not_implemented("the geometry types of parquet geospatial statistics"))
}

/// How many pages of each type and encoding a chunk has, and NULL when the writer did not say.
fn encoding_stats(meta: &Node) -> Value {
    if meta.get(13).is_none() {
        return Value::Null;
    }
    let values = meta
        .items(13)
        .iter()
        .map(|entry| {
            let tally = entry.int(3).unwrap_or(0);
            Value::Struct(vec![
                ("page_type".to_string(), Value::Varchar(page_name(entry.int(1).unwrap_or(0)))),
                ("encoding".to_string(), Value::Varchar(encoding_name(entry.int(2).unwrap_or(0)))),
                ("count".to_string(), Value::Integer(i32::try_from(tally).unwrap_or(i32::MAX))),
            ])
        })
        .collect();
    Value::List { element: encoding_stat(), values }
}

/// `parquet_schema`: a row per schema element, in file order.
fn schema_rows(footer: &RawFooter, path: &str) -> Result<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    for (column_id, element) in footer.schema().iter().enumerate() {
        let duckdb_type = match element.int(1) {
            Some(_) => Value::Varchar(derive(element)?.0.to_string()),
            None => Value::Null,
        };
        rows.push(vec![
            Value::Varchar(path.to_string()),
            Value::Varchar(element.text(4).unwrap_or_default()),
            varchar(element.int(1).map(physical_name)),
            element
                .int(2)
                .map_or(Value::Null, |len| Value::Integer(i32::try_from(len).unwrap_or(i32::MAX))),
            varchar(element.int(3).map(repetition_name)),
            big(element.int(5)),
            varchar(element.int(6).map(converted_name)),
            big(element.int(7)),
            big(element.int(8)),
            big(element.int(9)),
            varchar(element.get(10).and_then(logical_text)),
            duckdb_type,
            count(column_id),
        ]);
    }
    Ok(rows)
}

/// `parquet_file_metadata`'s one row.
fn file_row(footer: &RawFooter, path: &str) -> Vec<Value> {
    let root = &footer.root;
    let orders = root.get(7).map(|_| Value::List {
        element: LogicalType::Varchar,
        values: root
            .items(7)
            .iter()
            .map(|order| {
                let kind = order.get(1).map_or("<null>", |_| "TypeDefinedOrder()");
                Value::Varchar(format!("ColumnOrder(TYPE_ORDER={kind})"))
            })
            .collect(),
    });
    vec![
        Value::Varchar(path.to_string()),
        varchar(root.text(6)),
        Value::BigInt(root.int(3).unwrap_or(0)),
        count(root.items(4).len()),
        Value::BigInt(root.int(1).unwrap_or(0)),
        varchar(root.get(8).map(encryption_text)),
        varchar(root.text(9)),
        Value::UBigInt(footer.file_size),
        Value::UBigInt(footer.footer_size),
        orders.unwrap_or(Value::Null),
    ]
}

/// The Thrift text of an `EncryptionAlgorithm`.
fn encryption_text(node: &Node) -> String {
    let aes = |id: i16| match node.get(id) {
        None => "<null>".to_string(),
        Some(inner) => {
            let binary = |id: i16| inner.text(id).unwrap_or_else(|| "<null>".to_string());
            let supply =
                inner.flag(3).map_or("<null>".to_string(), |set| u8::from(set).to_string());
            let name = if id == 1 { "AesGcmV1" } else { "AesGcmCtrV1" };
            format!(
                "{name}(aad_prefix={}, aad_file_unique={}, supply_aad_prefix={supply})",
                binary(1),
                binary(2)
            )
        }
    };
    format!("EncryptionAlgorithm(AES_GCM_V1={}, AES_GCM_CTR_V1={})", aes(1), aes(2))
}

/// The Thrift text of the one member of a `LogicalType` union that is set, in the order the pin
/// looks for them, and nothing when none of those is.
fn logical_text(node: &Node) -> Option<String> {
    let empty = [(1, "StringType()"), (2, "MapType()"), (3, "ListType()"), (4, "EnumType()")];
    for (id, text) in empty {
        if node.get(id).is_some() {
            return Some(text.to_string());
        }
    }
    if let Some(decimal) = node.get(5) {
        let scale = decimal.int(1).unwrap_or(0);
        let precision = decimal.int(2).unwrap_or(0);
        return Some(format!("DecimalType(scale={scale}, precision={precision})"));
    }
    if node.get(6).is_some() {
        return Some("DateType()".to_string());
    }
    for (id, name) in [(7, "TimeType"), (8, "TimestampType")] {
        if let Some(time) = node.get(id) {
            let utc = u8::from(time.flag(1).unwrap_or(false));
            let unit = time.child(2);
            let part = |id: i16, text: &str| match unit.get(id) {
                Some(_) => text.to_string(),
                None => "<null>".to_string(),
            };
            return Some(format!(
                "{name}(isAdjustedToUTC={utc}, unit=TimeUnit(MILLIS={}, MICROS={}, NANOS={}))",
                part(1, "MilliSeconds()"),
                part(2, "MicroSeconds()"),
                part(3, "NanoSeconds()")
            ));
        }
    }
    if let Some(integer) = node.get(10) {
        let width = integer.int(1).unwrap_or(0);
        let signed = u8::from(integer.flag(2).unwrap_or(false));
        return Some(format!("IntType(bitWidth={width}, isSigned={signed})"));
    }
    let empty = [
        (11, "NullType()"),
        (12, "JsonType()"),
        (13, "BsonType()"),
        (14, "UUIDType()"),
        (15, "Float16Type()"),
    ];
    for (id, text) in empty {
        if node.get(id).is_some() {
            return Some(text.to_string());
        }
    }
    let crs = |inner: &Node| inner.text(1).unwrap_or_else(|| "<null>".to_string());
    if let Some(geometry) = node.get(17) {
        return Some(format!("GeometryType(crs={})", crs(geometry)));
    }
    if let Some(geography) = node.get(18) {
        let algorithm = geography.int(2).map_or("<null>".to_string(), |wire| {
            let names = ["SPHERICAL", "VINCENTY", "THOMAS", "ANDOYER", "KARNEY"];
            enum_name(&names, wire)
        });
        return Some(format!("GeographyType(crs={}, algorithm={algorithm})", crs(geography)));
    }
    None
}

/// The name Thrift prints for an enum value, and the number when the value has no name.
fn enum_name(names: &[&str], wire: i64) -> String {
    usize::try_from(wire)
        .ok()
        .and_then(|at| names.get(at))
        .filter(|name| !name.is_empty())
        .map_or_else(|| wire.to_string(), |name| (*name).to_string())
}

fn physical_name(wire: i64) -> String {
    let names = [
        "BOOLEAN",
        "INT32",
        "INT64",
        "INT96",
        "FLOAT",
        "DOUBLE",
        "BYTE_ARRAY",
        "FIXED_LEN_BYTE_ARRAY",
    ];
    enum_name(&names, wire)
}

fn repetition_name(wire: i64) -> String {
    enum_name(&["REQUIRED", "OPTIONAL", "REPEATED"], wire)
}

fn converted_name(wire: i64) -> String {
    let names = [
        "UTF8",
        "MAP",
        "MAP_KEY_VALUE",
        "LIST",
        "ENUM",
        "DECIMAL",
        "DATE",
        "TIME_MILLIS",
        "TIME_MICROS",
        "TIMESTAMP_MILLIS",
        "TIMESTAMP_MICROS",
        "UINT_8",
        "UINT_16",
        "UINT_32",
        "UINT_64",
        "INT_8",
        "INT_16",
        "INT_32",
        "INT_64",
        "JSON",
        "BSON",
        "INTERVAL",
        "",
        "",
        "PARQUET_NULL",
    ];
    enum_name(&names, wire)
}

fn codec_name(wire: i64) -> String {
    let names = ["UNCOMPRESSED", "SNAPPY", "GZIP", "LZO", "BROTLI", "LZ4", "ZSTD", "LZ4_RAW"];
    enum_name(&names, wire)
}

fn encoding_name(wire: i64) -> String {
    let names = [
        "PLAIN",
        "",
        "PLAIN_DICTIONARY",
        "RLE",
        "BIT_PACKED",
        "DELTA_BINARY_PACKED",
        "DELTA_LENGTH_BYTE_ARRAY",
        "DELTA_BYTE_ARRAY",
        "RLE_DICTIONARY",
        "BYTE_STREAM_SPLIT",
    ];
    enum_name(&names, wire)
}

fn page_name(wire: i64) -> String {
    enum_name(&["DATA_PAGE", "INDEX_PAGE", "DICTIONARY_PAGE", "DATA_PAGE_V2"], wire)
}

/// What else the pin needs to know about how a column is stored to read one of its statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Info {
    None,
    Millis,
    Micros,
    Nanos,
    Impala,
    Float16,
    DecimalInt32,
    DecimalInt64,
    DecimalBytes,
    /// A decimal wider than DuckDB's widest, read as a double at this scale.
    DecimalDouble(i64),
}

/// The type the pin reads a leaf as, and how it is stored, which is DuckDB's `DeriveLogicalType`
/// with `binary_as_string` off: the logical type for the few it looks at, then the converted type,
/// then the physical type.
fn derive(element: &Node) -> Result<(LogicalType, Info)> {
    let physical = element.int(1).unwrap_or(0);
    let length = element.int(2);
    if physical == 7 && length.is_none() {
        return Err(Error::io("FIXED_LEN_BYTE_ARRAY requires length to be set"));
    }
    if let Some(logical) = element.get(10) {
        if logical.get(11).is_some() {
            return Ok((LogicalType::Null, Info::None));
        } else if logical.get(14).is_some() {
            if physical == 7 {
                return Ok((LogicalType::Uuid, Info::None));
            }
        } else if logical.get(15).is_some() {
            if physical == 7 && length == Some(2) {
                return Ok((LogicalType::Float, Info::Float16));
            }
        } else if let Some(stamp) = logical.get(8) {
            let unit = unit(stamp, "TIMESTAMP")?;
            let ty = match (stamp.flag(1).unwrap_or(false), unit == Info::Nanos) {
                (true, true) => LogicalType::TimestampTzNs,
                (true, false) => LogicalType::TimestampTz,
                (false, true) => LogicalType::TimestampNs,
                (false, false) => LogicalType::Timestamp,
            };
            return Ok((ty, unit));
        } else if let Some(time) = logical.get(7) {
            let unit = unit(time, "TIME")?;
            let ty = match (time.flag(1).unwrap_or(false), unit == Info::Nanos) {
                (true, _) => LogicalType::TimeTz,
                (false, true) => LogicalType::TimeNs,
                (false, false) => LogicalType::Time,
            };
            return Ok((ty, unit));
        }
    }
    let Some(converted) = element.int(6) else {
        return Ok(match physical {
            0 => (LogicalType::Boolean, Info::None),
            1 => (LogicalType::Integer, Info::None),
            2 => (LogicalType::BigInt, Info::None),
            3 => (LogicalType::Timestamp, Info::Impala),
            4 => (LogicalType::Float, Info::None),
            5 => (LogicalType::Double, Info::None),
            6 | 7 => (LogicalType::Blob, Info::None),
            other => return Err(Error::io(format!("Unsupported parquet type ({other})"))),
        });
    };
    let only = |wanted: i64, ty: LogicalType, info: Info, message: &str| {
        if physical == wanted { Ok((ty, info)) } else { Err(Error::io(message.to_string())) }
    };
    let int32 =
        |name: &str| format!("{name} converted type can only be set for value of Type::INT32");
    let int64 =
        |name: &str| format!("{name} converted type can only be set for value of Type::INT64");
    match converted {
        24 => Ok((LogicalType::Null, Info::None)),
        15 => only(1, LogicalType::TinyInt, Info::None, &int32("INT8")),
        16 => only(1, LogicalType::SmallInt, Info::None, &int32("INT16")),
        17 => only(1, LogicalType::Integer, Info::None, &int32("INT32")),
        // The pin's message says INT32 here too.
        18 => only(2, LogicalType::BigInt, Info::None, &int32("INT64")),
        11 => only(1, LogicalType::UTinyInt, Info::None, &int32("UINT8")),
        12 => only(1, LogicalType::USmallInt, Info::None, &int32("UINT16")),
        13 => only(1, LogicalType::UInteger, Info::None, &int32("UINT32")),
        14 => only(2, LogicalType::UBigInt, Info::None, &int64("UINT64")),
        6 => only(1, LogicalType::Date, Info::None, &int32("DATE")),
        10 => only(2, LogicalType::Timestamp, Info::Micros, &int64("TIMESTAMP")),
        9 => only(2, LogicalType::Timestamp, Info::Millis, &int64("TIMESTAMP")),
        5 => {
            let (Some(precision), Some(scale)) = (element.int(8), element.int(7)) else {
                return Err(Error::io("DECIMAL requires a length and scale specifier!"));
            };
            if precision < 1 || scale < 0 || scale > precision {
                return Err(Error::io(format!(
                    "Invalid DECIMAL precision {precision} and scale {scale}"
                )));
            }
            if precision > 38 {
                return Ok((LogicalType::Double, Info::DecimalDouble(scale)));
            }
            let info = match physical {
                6 | 7 => Info::DecimalBytes,
                1 => Info::DecimalInt32,
                2 => Info::DecimalInt64,
                _ => {
                    return Err(Error::io(
                        "DECIMAL converted type can only be set for value of \
                         Type::(FIXED_LEN_)BYTE_ARRAY/INT32/INT64",
                    ));
                }
            };
            // Both are at most 38 here, checked above.
            let width = u8::try_from(precision).unwrap_or(38);
            let scale = u8::try_from(scale).unwrap_or(38);
            Ok((LogicalType::Decimal { width, scale }, info))
        }
        0 | 4 => match physical {
            6 | 7 => Ok((LogicalType::Varchar, Info::None)),
            _ => Err(Error::io(
                "UTF8 converted type can only be set for Type::(FIXED_LEN_)BYTE_ARRAY",
            )),
        },
        7 => only(1, LogicalType::Time, Info::Millis, &int32("TIME_MILLIS")),
        8 => only(2, LogicalType::Time, Info::Micros, &int64("TIME_MICROS")),
        21 => Ok((LogicalType::Interval, Info::None)),
        19 => Ok((LogicalType::Json, Info::None)),
        other => Err(Error::io(format!("Unsupported converted type ({other})"))),
    }
}

/// The unit of a `TIME` or a `TIMESTAMP` logical type.
fn unit(node: &Node, what: &str) -> Result<Info> {
    let unit = node.child(2);
    if unit.get(1).is_some() {
        Ok(Info::Millis)
    } else if unit.get(2).is_some() {
        Ok(Info::Micros)
    } else if unit.get(3).is_some() {
        Ok(Info::Nanos)
    } else {
        Err(Error::not_implemented(format!("Unimplemented {what} encoding - missing UNIT")))
    }
}

/// The statistic `bytes` as a value of the column's type, which is DuckDB's
/// `ParquetStatisticsUtils::ConvertValue`: NULL for an absent one, a float that is not finite or a
/// decimal that does not fit its width, and an error for one that is not the size of its type.
fn convert(ty: &LogicalType, info: Info, bytes: Option<&[u8]>) -> Result<Value> {
    let Some(bytes) = bytes else {
        return Ok(Value::Null);
    };
    let wrong = |name: &str| Error::invalid_input(format!("Incorrect stats size for type {name}"));
    let fixed = |name: &str, len: usize| {
        if bytes.len() == len { Ok(bytes) } else { Err(wrong(name)) }
    };
    let int32 = |name: &str| -> Result<i32> {
        let held = fixed(name, 4)?;
        Ok(i32::from_le_bytes([held[0], held[1], held[2], held[3]]))
    };
    let int64 = |name: &str| -> Result<i64> {
        let mut held = [0_u8; 8];
        held.copy_from_slice(fixed(name, 8)?);
        Ok(i64::from_le_bytes(held))
    };
    // A TIME is stored in four bytes or in eight.
    let either = |name: &str| -> Result<i64> {
        match bytes.len() {
            4 => int32(name).map(i64::from),
            8 => int64(name),
            _ => Err(wrong(name)),
        }
    };
    let finite = |value: f64, made: Value| if value.is_finite() { made } else { Value::Null };
    Ok(match ty {
        LogicalType::Boolean => Value::Boolean(fixed("BOOLEAN", 1)?[0] != 0),
        LogicalType::UTinyInt | LogicalType::USmallInt | LogicalType::UInteger => {
            #[expect(clippy::cast_sign_loss, reason = "the bytes are an unsigned 32 bit integer")]
            let value = int32("UINTEGER")? as u32;
            match ty {
                LogicalType::UTinyInt => u8::try_from(value).map_or(Value::Null, Value::UTinyInt),
                LogicalType::USmallInt => {
                    u16::try_from(value).map_or(Value::Null, Value::USmallInt)
                }
                _ => Value::UInteger(value),
            }
        }
        LogicalType::UBigInt => {
            #[expect(clippy::cast_sign_loss, reason = "the bytes are an unsigned 64 bit integer")]
            let value = int64("UBIGINT")? as u64;
            Value::UBigInt(value)
        }
        LogicalType::TinyInt => i8::try_from(int32("INTEGER")?).map_or(Value::Null, Value::TinyInt),
        LogicalType::SmallInt => {
            i16::try_from(int32("INTEGER")?).map_or(Value::Null, Value::SmallInt)
        }
        LogicalType::Integer => Value::Integer(int32("INTEGER")?),
        LogicalType::BigInt => Value::BigInt(int64("BIGINT")?),
        LogicalType::Float => {
            let value = if info == Info::Float16 {
                let held = fixed("FLOAT16", 2)?;
                half(u16::from_le_bytes([held[0], held[1]]))
            } else {
                let held = fixed("FLOAT", 4)?;
                f32::from_le_bytes([held[0], held[1], held[2], held[3]])
            };
            finite(f64::from(value), Value::Float(value))
        }
        LogicalType::Double => {
            if let Info::DecimalDouble(scale) = info {
                return Ok(Value::Double(decimal_double(bytes, scale)));
            }
            let value = f64::from_bits(int64("DOUBLE")?.cast_unsigned());
            finite(value, Value::Double(value))
        }
        LogicalType::Decimal { width, scale } => {
            let name = ty.to_string();
            let unscaled = match info {
                Info::DecimalInt32 => i128::from(int32(&name)?),
                Info::DecimalInt64 => i128::from(int64(&name)?),
                _ => {
                    let size = match width {
                        0..=4 => 2,
                        5..=9 => 4,
                        10..=18 => 8,
                        _ => 16,
                    };
                    decimal_bytes(bytes, size)?
                }
            };
            let limit = 10_i128.pow(u32::from(*width));
            if unscaled >= limit || unscaled <= -limit {
                Value::Null
            } else {
                Value::Decimal { unscaled, width: *width, scale: *scale }
            }
        }
        LogicalType::Varchar | LogicalType::Json => match std::str::from_utf8(bytes) {
            Ok(text) => Value::Varchar(text.to_string()),
            Err(_) => Value::Blob(bytes.to_vec()),
        },
        LogicalType::Blob => Value::Blob(bytes.to_vec()),
        LogicalType::Date => Value::Date(int32("DATE")?),
        LogicalType::Time => {
            let value = either("TIME")?;
            if info == Info::Millis {
                within(value, 86_400_000).map_or(Value::Null, |ms| Value::Time(ms * 1_000))
            } else {
                within(value, 86_400_000_000).map_or(Value::Null, Value::Time)
            }
        }
        LogicalType::TimeNs => {
            within(int64("TIME_NS")?, 86_400_000_000_000).map_or(Value::Null, Value::TimeNs)
        }
        LogicalType::TimeTz => {
            let value = either("TIMETZ")?;
            let micros = match info {
                Info::Millis => wrap(value, 86_400_000) * 1_000,
                Info::Nanos => wrap(value, 86_400_000_000_000) / 1_000,
                _ => wrap(value, 86_400_000_000),
            };
            Value::TimeTz(rudb_common::time_tz::pack(micros, 0))
        }
        LogicalType::Timestamp | LogicalType::TimestampTz => {
            let micros = if info == Info::Impala {
                let held = fixed("TIMESTAMP", 12)?;
                let mut nanos = [0_u8; 8];
                nanos.copy_from_slice(&held[..8]);
                let day = u32::from_le_bytes([held[8], held[9], held[10], held[11]]);
                (i64::from(day) - 2_440_588) * 86_400_000_000 + i64::from_le_bytes(nanos) / 1_000
            } else {
                let value = int64("TIMESTAMP")?;
                if info == Info::Millis && value != i64::MAX && value != -i64::MAX {
                    value.checked_mul(1_000).ok_or_else(|| {
                        Error::conversion("Could not convert Timestamp(MS) to Timestamp(US)")
                    })?
                } else {
                    value
                }
            };
            if *ty == LogicalType::TimestampTz {
                Value::TimestampTz(micros)
            } else {
                Value::Timestamp(micros)
            }
        }
        LogicalType::TimestampNs => Value::TimestampNs(int64("TIMESTAMP_NS")?),
        LogicalType::TimestampTzNs => Value::TimestampTzNs(int64("TIMESTAMP_NS")?),
        LogicalType::Uuid => {
            let mut held = [0_u8; 16];
            held.copy_from_slice(fixed("UUID", 16)?);
            Value::Uuid(rudb_common::uuid::from_bytes(held))
        }
        // The pin has no conversion for the rest and stops with an internal error, which is
        // worse than a NULL for a table that only prints the footer.
        _ => Value::Null,
    })
}

/// A time of day that has to be within a day, as the pin checks a TIME statistic.
fn within(value: i64, day: i64) -> Option<i64> {
    (0..=day).contains(&value).then_some(value)
}

/// A time of day taken modulo a day, with the end of the day kept, as the pin reads a TIMETZ.
fn wrap(value: i64, day: i64) -> i64 {
    if value == day { value } else { value.rem_euclid(day) }
}

/// A half precision float, widened.
fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f32::from(bits & 0x3ff);
    match exponent {
        0 => sign * fraction * 2_f32.powi(-24),
        31 if fraction == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + fraction / 1024.0) * 2_f32.powi(exponent - 15),
    }
}

/// A big endian two's complement decimal of any length, read into an integer `size` bytes wide
/// the way DuckDB's `ReadDecimalValue` does, including its error for bytes that do not fit.
fn decimal_bytes(bytes: &[u8], size: usize) -> Result<i128> {
    let Some(first) = bytes.first() else {
        return Ok(0);
    };
    let positive = first & 0x80 == 0;
    let flip = |byte: u8| if positive { byte } else { byte ^ 0xff };
    let mut held: u128 = 0;
    for (at, byte) in bytes.iter().rev().take(size).enumerate() {
        held |= u128::from(flip(*byte)) << (8 * at);
    }
    if bytes.iter().rev().skip(size).any(|byte| flip(*byte) != 0) {
        return Err(Error::invalid_input("Invalid decimal encoding in Parquet file"));
    }
    // The integer is `size` bytes wide, so what was read is that wide and signed.
    let bits = 8 * u32::try_from(size).unwrap_or(16);
    let shift = 128 - bits;
    let value = (held.cast_signed() << shift) >> shift;
    let value = if positive { value } else { (-(value.wrapping_add(1)) << shift) >> shift };
    Ok(value)
}

/// A decimal too wide for DuckDB's widest decimal, read as a double and scaled.
fn decimal_double(bytes: &[u8], scale: i64) -> f64 {
    let Some(first) = bytes.first() else {
        return 0.0;
    };
    let positive = first & 0x80 == 0;
    let mut value = 0.0;
    for byte in bytes {
        let byte = if positive { *byte } else { byte ^ 0xff };
        value = value * 256.0 + f64::from(byte);
    }
    let scale = 10_f64.powi(i32::try_from(scale).unwrap_or(i32::MAX));
    if positive { value / scale } else { -((value + 1.0) / scale) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thrift::Writer;

    /// A footer with one INT32 column of two rows in one row group, written the way the pin
    /// writes one, with the statistics and the encoding counts it keeps.
    fn footer() -> RawFooter {
        let mut root = Writer::default();
        root.i32(1, 1);
        let mut top = Writer::default();
        top.i32(3, 0);
        top.string(4, "duckdb_schema");
        top.i32(5, 1);
        let mut leaf = Writer::default();
        leaf.i32(1, 1);
        leaf.i32(3, 1);
        leaf.string(4, "i");
        leaf.i32(6, 17);
        let mut integer = Writer::default();
        integer.field(1, 3);
        integer.varint(32);
        integer.boolean(2, true);
        let mut logical = Writer::default();
        logical.nested(10, integer);
        leaf.nested(10, logical);
        root.list_of_nested(2, vec![top, leaf]);
        root.int(3, 2);
        let mut stats = Writer::default();
        stats.binary(1, &7_i32.to_le_bytes());
        stats.binary(2, &(-3_i32).to_le_bytes());
        stats.int(3, 0);
        stats.binary(5, &7_i32.to_le_bytes());
        stats.binary(6, &(-3_i32).to_le_bytes());
        stats.boolean(7, true);
        stats.boolean(8, true);
        let mut meta = Writer::default();
        meta.i32(1, 1);
        meta.list_of_ints(2, &[0]);
        meta.list_of_strings(3, &["i"]);
        meta.i32(4, 1);
        meta.int(5, 2);
        meta.int(6, 30);
        meta.int(7, 32);
        meta.int(9, 4);
        meta.nested(12, stats);
        let mut tally = Writer::default();
        tally.i32(1, 0);
        tally.i32(2, 0);
        tally.i32(3, 1);
        meta.list_of_nested(13, vec![tally]);
        let mut chunk = Writer::default();
        chunk.int(2, 4);
        chunk.nested(3, meta);
        let mut group = Writer::default();
        group.list_of_nested(1, vec![chunk]);
        group.int(2, 30);
        group.int(3, 2);
        group.int(5, 4);
        group.int(6, 32);
        root.list_of_nested(4, vec![group]);
        let mut pair = Writer::default();
        pair.string(1, "k");
        pair.string(2, "v");
        root.list_of_nested(5, vec![pair]);
        root.string(6, "DuckDB");
        let mut order = Writer::default();
        order.nested(1, Writer::default());
        root.list_of_nested(7, vec![order]);
        let bytes = root.stop();
        RawFooter::parse(&bytes, 100).expect("a footer")
    }

    #[test]
    fn a_column_chunk_prints_as_the_pin_prints_it() {
        let rows = MetadataTable::Columns.rows(&footer(), "f.parquet").expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.len(), MetadataTable::Columns.fields().len());
        assert_eq!(row[6], Value::BigInt(4));
        assert_eq!(row[8], Value::Varchar("i".into()));
        assert_eq!(row[9], Value::Varchar("INT32".into()));
        assert_eq!(row[10], Value::Integer(-3));
        assert_eq!(row[11], Value::Integer(7));
        assert_eq!(row[13], Value::Null);
        assert_eq!(row[16], Value::Varchar("SNAPPY".into()));
        assert_eq!(row[17], Value::Varchar("PLAIN".into()));
        assert_eq!(row[19], Value::Null);
        assert_eq!(row[26], Value::Boolean(true));
        assert_eq!(row[28], Value::BigInt(32));
        assert_eq!(row[29], Value::Null);
        assert_eq!(
            row[31].to_string(),
            "[{'page_type': DATA_PAGE, 'encoding': PLAIN, 'count': 1}]"
        );
        assert_eq!(row[32], Value::Null);
    }

    #[test]
    fn a_schema_element_prints_its_thrift_text_and_its_duckdb_type() {
        let rows = MetadataTable::Schema.rows(&footer(), "f.parquet").expect("rows");
        assert_eq!(rows[0][2], Value::Null);
        assert_eq!(rows[0][4], Value::Varchar("REQUIRED".into()));
        assert_eq!(rows[0][11], Value::Null);
        assert_eq!(rows[1][6], Value::Varchar("INT_32".into()));
        assert_eq!(rows[1][10], Value::Varchar("IntType(bitWidth=32, isSigned=1)".into()));
        assert_eq!(rows[1][11], Value::Varchar("INTEGER".into()));
        assert_eq!(rows[1][12], Value::BigInt(1));
    }

    #[test]
    fn the_file_and_its_keys_print_one_row_and_a_row_a_key() {
        let footer = footer();
        let file = MetadataTable::File.rows(&footer, "f.parquet").expect("rows");
        assert_eq!(file[0][1], Value::Varchar("DuckDB".into()));
        assert_eq!(file[0][3], Value::BigInt(1));
        assert_eq!(file[0][7], Value::UBigInt(100));
        assert_eq!(file[0][9].to_string(), "['ColumnOrder(TYPE_ORDER=TypeDefinedOrder())']");
        let pairs = MetadataTable::KeyValue.rows(&footer, "f.parquet").expect("rows");
        assert_eq!(
            pairs,
            [[
                Value::Varchar("f.parquet".into()),
                Value::Blob(b"k".to_vec()),
                Value::Blob(b"v".to_vec())
            ]]
        );
    }

    #[test]
    fn a_statistic_reads_as_its_column_type_or_as_null() {
        let decimal = LogicalType::Decimal { width: 5, scale: 2 };
        let bytes = [0xff_u8, 0x85];
        assert_eq!(
            convert(&decimal, Info::DecimalBytes, Some(&bytes)).expect("a decimal"),
            Value::Decimal { unscaled: -123, width: 5, scale: 2 }
        );
        let wide = 100_000_i32.to_le_bytes();
        assert_eq!(
            convert(&decimal, Info::DecimalInt32, Some(&wide)).expect("a decimal"),
            Value::Null
        );
        let nan = f64::NAN.to_le_bytes();
        assert_eq!(
            convert(&LogicalType::Double, Info::None, Some(&nan)).expect("a double"),
            Value::Null
        );
        let big = 300_u32.to_le_bytes();
        assert_eq!(
            convert(&LogicalType::UTinyInt, Info::None, Some(&big)).expect("a byte"),
            Value::Null
        );
        let error = convert(&LogicalType::BigInt, Info::None, Some(&big)).expect_err("too short");
        assert_eq!(error.message(), "Incorrect stats size for type BIGINT");
        assert_eq!(half(0x3c00), 1.0);
        assert_eq!(half(0xc000), -2.0);
    }
}
