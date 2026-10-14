//! The Parquet footer: what columns a file has, where their pages are, and what is in them.
//!
//! A Parquet file ends with its own table of contents, and reading a file starts at the end: the
//! last four bytes are the magic `PAR1`, the four before those are how long the footer is, and the
//! footer is a Thrift structure describing every row group and every column chunk in the file. That
//! layout is what makes a Parquet reader able to read one column of a hundred without touching the
//! other ninety nine, because the footer says where that column's bytes are in every row group and
//! the reader goes straight to them.
//!
//! Everything in here is the footer and nothing in here is data. The structures are the ones
//! `parquet.thrift` defines, narrowed to the fields a reader uses, and a field this version does
//! not read is skipped rather than rejected. A column below the root that is a group, or that is
//! repeated, is read as the tree it is, with the definition and repetition levels of every node
//! worked out here, because a level the reader got wrong would be a wrong answer and not a missing
//! feature.
//!
//! Sizes are read as the file states them and not trusted. An offset out of the file, a negative
//! count and a page longer than the chunk that contains it are all things a corrupt file can say,
//! and each one is checked where it is used rather than assumed away here.

use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;

use rudb_common::bounds::End;
use rudb_common::{Error, Field, LogicalType, Result};
use rudb_compress::Codec;
use rudb_io::File;

use crate::thrift::{Kind, Reader};

/// The four bytes at each end of a Parquet file.
const MAGIC: &[u8; 4] = b"PAR1";

/// How the values of a column are physically stored.
///
/// Parquet has seven of these and everything else is an annotation on top of one of them, which is
/// why a reader dispatches on the physical type and consults the logical type only to decide what
/// to call the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Physical {
    /// One bit a value, bit packed.
    Boolean,
    /// Four bytes, little endian, signed.
    Int32,
    /// Eight bytes, little endian, signed.
    Int64,
    /// Twelve bytes, a legacy timestamp nobody should write any more.
    Int96,
    /// Four bytes, IEEE 754.
    Float,
    /// Eight bytes, IEEE 754.
    Double,
    /// A length and then that many bytes.
    ByteArray,
    /// A fixed number of bytes, stated in the schema.
    FixedLenByteArray,
}

impl Physical {
    /// The physical type with this wire value.
    fn from_wire(wire: i64) -> Result<Self> {
        Ok(match wire {
            0 => Self::Boolean,
            1 => Self::Int32,
            2 => Self::Int64,
            3 => Self::Int96,
            4 => Self::Float,
            5 => Self::Double,
            6 => Self::ByteArray,
            7 => Self::FixedLenByteArray,
            other => {
                return Err(Error::io(format!(
                    "a parquet physical type of {other}, which is not one"
                )));
            }
        })
    }
}

/// How the values in a page are encoded.
///
/// `PlainDictionary` and `RleDictionary` are the same thing written by writers of different ages.
/// The first spells the dictionary page's own encoding `PLAIN_DICTIONARY` and the indices the same
/// way, and the second calls the page `PLAIN` and the indices `RLE_DICTIONARY`. Both put a one byte
/// bit width in front of the indices and then the hybrid run length encoding, so the decoder is one
/// decoder and only the enum has two names in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Values as themselves.
    Plain,
    /// Dictionary indices, in the older spelling.
    PlainDictionary,
    /// The hybrid run length and bit packed encoding, which is also how levels are written.
    Rle,
    /// The bit packing the format deprecated, used for levels by very old writers.
    BitPacked,
    /// Delta encoded integers.
    DeltaBinaryPacked,
    /// Byte arrays with their lengths delta encoded and their bytes concatenated.
    DeltaLengthByteArray,
    /// Byte arrays that share a prefix with the value before them.
    DeltaByteArray,
    /// Dictionary indices, in the newer spelling.
    RleDictionary,
    /// Floating point split into one stream per byte position.
    ByteStreamSplit,
}

impl Encoding {
    /// The encoding with this wire value.
    pub(crate) fn from_wire(wire: i64) -> Result<Self> {
        Ok(match wire {
            0 => Self::Plain,
            2 => Self::PlainDictionary,
            3 => Self::Rle,
            4 => Self::BitPacked,
            5 => Self::DeltaBinaryPacked,
            6 => Self::DeltaLengthByteArray,
            7 => Self::DeltaByteArray,
            8 => Self::RleDictionary,
            9 => Self::ByteStreamSplit,
            other => {
                return Err(Error::io(format!("a parquet encoding of {other}, which is not one")));
            }
        })
    }

    /// What the encoding is called, for an error message that has to name one.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "PLAIN",
            Self::PlainDictionary => "PLAIN_DICTIONARY",
            Self::Rle => "RLE",
            Self::BitPacked => "BIT_PACKED",
            Self::DeltaBinaryPacked => "DELTA_BINARY_PACKED",
            Self::DeltaLengthByteArray => "DELTA_LENGTH_BYTE_ARRAY",
            Self::DeltaByteArray => "DELTA_BYTE_ARRAY",
            Self::RleDictionary => "RLE_DICTIONARY",
            Self::ByteStreamSplit => "BYTE_STREAM_SPLIT",
        }
    }

    /// Whether this encoding reads its values out of a dictionary page.
    #[must_use]
    pub const fn is_dictionary(self) -> bool {
        matches!(self, Self::PlainDictionary | Self::RleDictionary)
    }
}

/// One column of the schema, which is one child of its root.
///
/// For a leaf that is the column the file stores. For a group, or a leaf the file repeats, it is
/// the tree in [`SchemaColumn::nested`], and the physical type is the first leaf's, which nothing
/// decodes by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    /// What the column is called.
    pub name: String,
    /// How it is stored.
    pub physical: Physical,
    /// What rudb calls it, which is the physical type plus whatever the file annotates it with.
    pub ty: LogicalType,
    /// Whether a value may be missing, which is Parquet's `OPTIONAL` against its `REQUIRED`.
    pub optional: bool,
    /// The declared length, for `FIXED_LEN_BYTE_ARRAY` and zero for everything else.
    pub width: i32,
    /// The unit a time or a timestamp is stored at, which its values and bounds are read from
    /// into the unit its type holds.
    pub(crate) unit: Option<Unit>,
    /// How the column is put together out of the leaves that store it, when it is not one of them.
    pub nested: Option<Arc<Nested>>,
}

/// A column that is stored in leaves below it, and the levels that say how they fit together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nested {
    /// The column itself.
    pub root: Node,
    /// The leaves, in the order the file stores them.
    pub leaves: Vec<Leaf>,
}

/// One leaf of a nested column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// The leaf as a flat column, which is what its pages are decoded as.
    pub column: SchemaColumn,
    /// The names from the column down to the leaf, which is what a column chunk is matched by.
    pub path: Vec<String>,
    /// The definition level of a value that is there.
    pub max_def: u32,
    /// The repetition level of a value that is not the first of the innermost list it is in.
    pub max_rep: u32,
}

/// One node of a nested column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The definition level at which this node is there, which is its parent's when it is required.
    pub def: u32,
    /// The repetition level of the innermost list this node is in, which is zero outside of one.
    ///
    /// A level after this node's first that is above this one belongs to this node still. That is
    /// how DuckDB writes a fixed size array that is null, with an entry for every element of it.
    pub rep: u32,
    /// Which of [`Nested::leaves`] are below this node.
    pub leaves: Range<usize>,
    /// What the node is.
    pub shape: Shape,
}

/// What a node of a nested column is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// The leaf at this place in [`Nested::leaves`].
    Leaf(usize),
    /// A struct, with a node per field.
    Struct(Vec<Node>),
    /// A list, which holds an element at definition level `def` and repeats at level `rep`.
    List {
        /// The definition level at which the list is there and not empty.
        def: u32,
        /// The repetition level of every element after the first.
        rep: u32,
        /// The element.
        element: Box<Node>,
    },
    /// A map, which is a list whose element is a key and a value.
    Map {
        /// The definition level at which the map is there and not empty.
        def: u32,
        /// The repetition level of every entry after the first.
        rep: u32,
        /// The key.
        key: Box<Node>,
        /// The value.
        value: Box<Node>,
    },
}

/// What a writer said about the values in one column chunk.
///
/// The bounds are the raw bytes rather than values, because interpreting them needs the column's
/// type and a reader that only wants to skip a row group does not have to interpret them at all.
/// The older `min` and `max` fields are not read: writers disagreed about how they ordered strings,
/// which is exactly why `min_value` and `max_value` were added, and a pruning decision taken on a
/// bound whose ordering is in doubt is a wrong answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// How many values in the chunk are null, when the writer counted them.
    pub nulls: Option<i64>,
    /// How many distinct values the chunk holds, when the writer counted them.
    ///
    /// Most writers leave this out, because counting it costs a pass that the rest of the statistics
    /// do not. DuckDB writes it for the columns it already has the number for, which in practice is
    /// the dictionary encoded ones, and those are the low cardinality columns that joins are written
    /// on. Nulls are not counted here, the same way `COUNT(DISTINCT)` does not count them.
    pub distinct: Option<i64>,
    /// The smallest value, in the column's plain encoding.
    pub min: Option<Vec<u8>>,
    /// The largest value, in the column's plain encoding.
    pub max: Option<Vec<u8>>,
    /// Whether [`Stats::min`] is the smallest value itself, where the writer said either way.
    ///
    /// A writer is allowed to shorten a long string bound as long as it moves it outward, so a
    /// shortened minimum is no larger than the smallest value and a shortened maximum is no smaller
    /// than the largest. That keeps every skip this makes correct, because a wider range rules out
    /// no more than a narrow one does, and it is exactly what stops the bound from being an answer:
    /// `MIN(url)` off a shortened minimum is a string that is not in the column.
    ///
    /// `None` is the ordinary state and means the writer said nothing, which most of them do. See
    /// [`Stats::exact`] for what is made of that, which is not the same as assuming the worst.
    pub min_exact: Option<bool>,
    /// Whether [`Stats::max`] is the largest value itself, where the writer said either way.
    pub max_exact: Option<bool>,
}

impl Stats {
    /// Whether the bound on `end` is the value itself rather than one the writer moved outward.
    ///
    /// The writer's word where there is one. Where there is not, it comes from the physical type:
    /// shortening is defined for byte strings and for nothing else, because a number has no prefix
    /// to keep and a writer that dropped half of an `INT64` would not have a bound at all. So the
    /// fixed width types are exact by construction and the two byte array types are not exact
    /// unless somebody said so.
    ///
    /// That is not caution for its own sake. The ClickBench file states neither flag on any of its
    /// twenty three thousand column chunks, and its dates and its integers are still answers under
    /// this rule while its strings are not. A file DuckDB wrote states `true` on all of them and
    /// its strings are answers too.
    #[must_use]
    pub fn exact(&self, end: End, physical: Physical) -> bool {
        let stated = match end {
            End::Low => self.min_exact,
            End::High => self.max_exact,
        };
        stated.unwrap_or(!matches!(physical, Physical::ByteArray | Physical::FixedLenByteArray))
    }

    /// The bytes of the bound on `end`, whatever the writer wrote there.
    #[must_use]
    pub fn bound(&self, end: End) -> Option<&[u8]> {
        match end {
            End::Low => self.min.as_deref(),
            End::High => self.max.as_deref(),
        }
    }
}

/// One column of one row group: where its bytes are and what is in them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChunk {
    /// Which column of the schema this is.
    pub column: usize,
    /// How it is stored, repeated from the schema so a page decoder needs one structure.
    pub physical: Physical,
    /// What the pages are compressed with.
    pub compression: Codec,
    /// Every encoding the writer said it used in this chunk.
    pub encodings: Vec<Encoding>,
    /// How many values are in the chunk, which counts nulls.
    pub values: i64,
    /// How many bytes the pages take in the file.
    pub compressed_size: i64,
    /// How many bytes the pages take once decompressed.
    pub uncompressed_size: i64,
    /// Where the first data page starts.
    pub data_page_offset: u64,
    /// Where the dictionary page starts, when there is one.
    pub dictionary_page_offset: Option<u64>,
    /// What the writer said about the values.
    pub stats: Option<Stats>,
    /// Where the column index for this chunk is, when the writer wrote one.
    ///
    /// The column index holds the same bounds [`ColumnChunk::stats`] holds, one set per page instead
    /// of one per chunk. It is not in the footer: the footer says where it is, and a reader that
    /// wants it reads it. That is the format doing the right thing, because a hundred and five
    /// columns times nine row groups times a bound per page is larger than the rest of the footer
    /// put together and a query wants it for the columns it filters on and no others.
    pub column_index: Option<Region>,
    /// Where the offset index for this chunk is, when the writer wrote one.
    ///
    /// Bounds without offsets are not actionable. The column index says page four cannot hold a
    /// matching row, and this is what says where page four starts and which row it starts at.
    pub offset_index: Option<Region>,
    /// The chunk of every leaf of a nested column, in the order of [`Nested::leaves`].
    ///
    /// Empty for a flat column. For a nested one the sizes above are the sum of these, the place is
    /// the first leaf's and there are no statistics, since a bound on a leaf is not a bound on the
    /// column it is in.
    pub leaves: Vec<ColumnChunk>,
}

/// A stretch of the file to read, as the footer describes it.
///
/// Two numbers with a name, which is worth a type because the footer has four of them in pairs and
/// crossing the pair is a bug a type can prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Where it starts.
    pub at: u64,
    /// How long it is.
    pub len: usize,
}

impl ColumnChunk {
    /// Where this chunk's bytes begin, which is the dictionary page when there is one.
    ///
    /// The dictionary page comes before the data pages and the two runs are contiguous, so this
    /// plus [`ColumnChunk::compressed_size`] is the one read that gets the whole column.
    #[must_use]
    pub fn start(&self) -> u64 {
        self.dictionary_page_offset.unwrap_or(self.data_page_offset)
    }
}

/// One row group: a horizontal slice of the file with every column in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGroup {
    /// One per column of the schema, in schema order.
    pub columns: Vec<ColumnChunk>,
    /// How many rows are in the group.
    pub rows: i64,
    /// How many bytes the group takes, uncompressed, as the writer counted it.
    pub bytes: i64,
}

/// A whole file's footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The format version the writer claimed.
    pub version: i32,
    /// How many rows are in the file, across every row group.
    pub rows: i64,
    /// The columns, flat, in the order the file stores them.
    pub schema: Vec<SchemaColumn>,
    /// The row groups, in file order.
    pub row_groups: Vec<RowGroup>,
    /// What wrote the file, which is the first thing to look at when a file decodes strangely.
    pub created_by: Option<String>,
}

impl Metadata {
    /// Reads the footer of an open file.
    ///
    /// Two reads: eight bytes at the end for the length and the magic, then the footer itself. The
    /// magic at the start of the file is checked as well, because a file that ends in `PAR1` and
    /// does not begin with it is a Parquet footer appended to something else.
    ///
    /// # Errors
    ///
    /// If the file is too short, is not Parquet, states a footer longer than itself, or the footer
    /// does not parse.
    pub fn read(file: &dyn File) -> Result<Self> {
        Self::parse(&footer(file)?)
    }

    /// Parses a footer that has already been read.
    ///
    /// Separate from [`Metadata::read`] so that a caller holding the bytes for another reason, a
    /// test or a cache, does not have to go back to the file.
    ///
    /// # Errors
    ///
    /// If the bytes are not a `FileMetaData`, or the schema is nested, or a column chunk names a
    /// column the schema does not have.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let mut version = 0;
        let mut rows = 0;
        let mut schema = Vec::new();
        let mut groups: Vec<RawGroup> = Vec::new();
        let mut created_by = None;
        while let Some(field) = reader.field_begin()? {
            match field.id {
                1 => version = reader.read_int()? as i32,
                2 => schema = read_schema(&mut reader)?,
                3 => rows = reader.read_int()?,
                4 => {
                    let (len, kind) = reader.list_begin()?;
                    expect(kind, Kind::Struct, "row groups")?;
                    groups.reserve(len);
                    for _ in 0..len {
                        groups.push(read_row_group(&mut reader)?);
                    }
                }
                6 => created_by = Some(reader.read_string()?.to_string()),
                _ => reader.skip(field.kind)?,
            }
        }
        if schema.is_empty() {
            return Err(Error::io("a parquet footer with no schema in it"));
        }
        let row_groups = groups
            .into_iter()
            .map(|group| resolve(group, &schema))
            .collect::<Result<Vec<RowGroup>>>()?;
        Ok(Self { version, rows, schema, row_groups, created_by })
    }

    /// The schema as rudb sees it, which is what a scan puts in a chunk.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.schema
            .iter()
            .map(|column| Field::new(column.name.clone(), column.ty.clone()))
            .collect()
    }

    /// Which column of the schema goes by this name, matched the way SQL matches it.
    ///
    /// Case insensitive, because `hits` is written in camel case and `SELECT advengineid FROM hits`
    /// is the same query as `SELECT AdvEngineID FROM hits`.
    #[must_use]
    pub fn column_named(&self, name: &str) -> Option<usize> {
        self.schema.iter().position(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// A one line per column summary, which is what a test compares and what a person reads.
    ///
    /// # Panics
    ///
    /// Never. The writes are into a `String`, which cannot fail, and the result is discarded.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(
            text,
            "{} rows in {} row groups, written by {}",
            self.rows,
            self.row_groups.len(),
            self.created_by.as_deref().unwrap_or("nobody in particular")
        );
        for (at, column) in self.schema.iter().enumerate() {
            let null = if column.optional { "optional" } else { "required" };
            let _ = writeln!(text, "{at} {} {} {null}", column.name, column.ty);
        }
        text
    }
}

/// A row group as the footer states it, before the column chunks are matched to the schema.
#[derive(Debug)]
struct RawGroup {
    columns: Vec<RawChunk>,
    rows: i64,
    bytes: i64,
}

/// A column chunk as the footer states it, holding the name path it has not been resolved by yet.
#[derive(Debug)]
struct RawChunk {
    path: Vec<String>,
    physical: Physical,
    compression: Codec,
    encodings: Vec<Encoding>,
    values: i64,
    compressed_size: i64,
    uncompressed_size: i64,
    data_page_offset: u64,
    dictionary_page_offset: Option<u64>,
    stats: Option<Stats>,
    column_index: Option<Region>,
    offset_index: Option<Region>,
}

/// Matches every chunk of a row group to the schema column it belongs to.
///
/// The chunks of a nested column's leaves become one chunk for the column, holding them.
fn resolve(group: RawGroup, schema: &[SchemaColumn]) -> Result<RowGroup> {
    let mut columns = Vec::with_capacity(group.columns.len());
    let mut leaves: Vec<Vec<Option<ColumnChunk>>> = schema
        .iter()
        .map(|column| {
            column.nested.as_ref().map_or(Vec::new(), |nested| vec![None; nested.leaves.len()])
        })
        .collect();
    for chunk in group.columns {
        let place = schema.iter().enumerate().find_map(|(at, column)| match &column.nested {
            None => (chunk.path.len() == 1 && column.name == chunk.path[0]).then_some((at, None)),
            Some(nested) => nested
                .leaves
                .iter()
                .position(|leaf| leaf.path == chunk.path)
                .map(|leaf| (at, Some(leaf))),
        });
        let Some((column, leaf)) = place else {
            return Err(Error::io(format!(
                "a column chunk for {}, which is not a column",
                chunk.path.join(".")
            )));
        };
        let resolved = ColumnChunk {
            column,
            physical: chunk.physical,
            compression: chunk.compression,
            encodings: chunk.encodings,
            values: chunk.values,
            compressed_size: chunk.compressed_size,
            uncompressed_size: chunk.uncompressed_size,
            data_page_offset: chunk.data_page_offset,
            dictionary_page_offset: chunk.dictionary_page_offset,
            stats: chunk.stats,
            column_index: chunk.column_index,
            offset_index: chunk.offset_index,
            leaves: Vec::new(),
        };
        match leaf {
            None => columns.push(resolved),
            Some(leaf) => leaves[column][leaf] = Some(resolved),
        }
    }
    for (column, parts) in leaves.into_iter().enumerate() {
        if parts.is_empty() {
            continue;
        }
        let parts = parts.into_iter().collect::<Option<Vec<ColumnChunk>>>().ok_or_else(|| {
            Error::io(format!("a row group missing a leaf of column {}", schema[column].name))
        })?;
        columns.push(combined(column, parts));
    }
    columns.sort_by_key(|chunk| chunk.column);
    Ok(RowGroup { columns, rows: group.rows, bytes: group.bytes })
}

/// The chunk of a nested column, out of the chunks of its leaves.
fn combined(column: usize, leaves: Vec<ColumnChunk>) -> ColumnChunk {
    let first = &leaves[0];
    let mut encodings = Vec::new();
    for leaf in &leaves {
        for encoding in &leaf.encodings {
            if !encodings.contains(encoding) {
                encodings.push(*encoding);
            }
        }
    }
    ColumnChunk {
        column,
        physical: first.physical,
        compression: first.compression,
        encodings,
        values: leaves.iter().map(|leaf| leaf.values).sum(),
        compressed_size: leaves.iter().map(|leaf| leaf.compressed_size).sum(),
        uncompressed_size: leaves.iter().map(|leaf| leaf.uncompressed_size).sum(),
        data_page_offset: first.data_page_offset,
        dictionary_page_offset: first.dictionary_page_offset,
        stats: None,
        column_index: None,
        offset_index: None,
        leaves,
    }
}

/// Reads the `schema` list, which is a tree written as a preorder walk.
///
/// The first element is the root and it is not a column. Every element after it carries how many
/// children it has. A child of the root with none, which the file does not repeat, is a flat
/// column, and any other child of the root is the tree below it.
fn read_schema(reader: &mut Reader<'_>) -> Result<Vec<SchemaColumn>> {
    let (len, kind) = reader.list_begin()?;
    expect(kind, Kind::Struct, "the schema")?;
    let mut elements = Vec::with_capacity(len);
    for _ in 0..len {
        elements.push(read_schema_element(reader)?);
    }
    let mut columns = Vec::with_capacity(len.saturating_sub(1));
    let mut at = 1;
    while at < elements.len() {
        let element = &elements[at];
        if element.children == 0 && element.repetition != REPEATED {
            let physical = element.physical.ok_or_else(|| {
                Error::io(format!("a leaf column named {} with no type", element.name))
            })?;
            let ty = logical_type(physical, element)?;
            columns.push(SchemaColumn {
                name: element.name.clone(),
                physical,
                ty,
                optional: element.repetition != 0,
                width: element.width,
                unit: unit(element),
                nested: None,
            });
            at += 1;
            continue;
        }
        let mut tree = Tree { elements: &elements, at, path: Vec::new(), leaves: Vec::new() };
        let (name, ty, root) = tree.field(0, 0)?;
        at = tree.at;
        let leaves = tree.leaves;
        let physical = leaves
            .first()
            .ok_or_else(|| Error::io(format!("a group column named {name} with no leaves")))?
            .column
            .physical;
        columns.push(SchemaColumn {
            name,
            physical,
            ty,
            optional: element.repetition == OPTIONAL,
            width: 0,
            unit: None,
            nested: Some(Arc::new(Nested { root, leaves })),
        });
    }
    Ok(columns)
}

/// `OPTIONAL` in the schema's `repetition_type`.
const OPTIONAL: i64 = 1;

/// `REPEATED` in the schema's `repetition_type`.
const REPEATED: i64 = 2;

/// The walk that turns the elements below one child of the root into a [`Nested`].
struct Tree<'a> {
    elements: &'a [SchemaElement],
    at: usize,
    path: Vec<String>,
    leaves: Vec<Leaf>,
}

impl Tree<'_> {
    /// The next element, as a field whose parent is there at level `def` inside lists at `rep`.
    ///
    /// A repeated element is a list of itself, which is how a file that does not use the `LIST`
    /// annotation writes one. Anything else is there at its parent's level, or one above that when
    /// it is optional.
    fn field(&mut self, def: u32, rep: u32) -> Result<(String, LogicalType, Node)> {
        let element = self.next()?;
        let repetition = self.elements[element].repetition;
        if repetition == REPEATED {
            let first = self.leaves.len();
            let (name, ty, inner) = self.body(element, def + 1, rep + 1)?;
            let shape = Shape::List { def: def + 1, rep: rep + 1, element: Box::new(inner) };
            let node = Node { def, rep, leaves: first..self.leaves.len(), shape };
            return Ok((name, LogicalType::List(Box::new(ty)), node));
        }
        let own = if repetition == OPTIONAL { def + 1 } else { def };
        self.body(element, own, rep)
    }

    /// Steps to the next element and says where it is.
    fn next(&mut self) -> Result<usize> {
        if self.at >= self.elements.len() {
            return Err(Error::io("a parquet schema with fewer elements than it says it has"));
        }
        self.at += 1;
        Ok(self.at - 1)
    }

    /// The node for the element at `element`, which is there at level `def` inside lists at `rep`.
    fn body(&mut self, element: usize, def: u32, rep: u32) -> Result<(String, LogicalType, Node)> {
        let first = self.leaves.len();
        let elements = self.elements;
        let element = &elements[element];
        self.path.push(element.name.clone());
        let (ty, shape) = if element.children <= 0 {
            let physical = element.physical.ok_or_else(|| {
                Error::io(format!("a leaf column named {} with no type", element.name))
            })?;
            let ty = logical_type(physical, element)?;
            let column = SchemaColumn {
                name: self.path.join("."),
                physical,
                ty: ty.clone(),
                optional: def > 0,
                width: element.width,
                unit: unit(element),
                nested: None,
            };
            self.leaves.push(Leaf { column, path: self.path.clone(), max_def: def, max_rep: rep });
            (ty, Shape::Leaf(first))
        } else if is_list(element) {
            self.list(element, def, rep)?
        } else if is_map(element) {
            self.map(element, def, rep)?
        } else {
            let mut fields = Vec::new();
            let mut nodes = Vec::new();
            for _ in 0..element.children {
                let (name, ty, node) = self.field(def, rep)?;
                fields.push(Field::new(name, ty));
                nodes.push(node);
            }
            (LogicalType::Struct(fields), Shape::Struct(nodes))
        };
        self.path.pop();
        let node = Node { def, rep, leaves: first..self.leaves.len(), shape };
        Ok((element.name.clone(), ty, node))
    }

    /// A group annotated as a list, whose one child is the repeated group.
    ///
    /// That child holds the element when it is a group of one, which is the three level form every
    /// current writer uses. Otherwise the child is the element itself, which is the two level form
    /// older writers left behind.
    fn list(&mut self, list: &SchemaElement, def: u32, rep: u32) -> Result<(LogicalType, Shape)> {
        let repeated = self.repeated(list)?;
        let (ty, element) = if self.elements[repeated].children == 1 {
            self.path.push(self.elements[repeated].name.clone());
            let (_, ty, element) = self.field(def + 1, rep + 1)?;
            self.path.pop();
            (ty, element)
        } else {
            let (_, ty, element) = self.body(repeated, def + 1, rep + 1)?;
            (ty, element)
        };
        let shape = Shape::List { def: def + 1, rep: rep + 1, element: Box::new(element) };
        Ok((LogicalType::List(Box::new(ty)), shape))
    }

    /// A group annotated as a map, whose one child is the repeated group of a key and a value.
    fn map(&mut self, map: &SchemaElement, def: u32, rep: u32) -> Result<(LogicalType, Shape)> {
        let repeated = self.repeated(map)?;
        if self.elements[repeated].children != 2 {
            return Err(Error::not_implemented(format!(
                "a parquet map named {} whose entries are not a key and a value",
                map.name
            )));
        }
        self.path.push(self.elements[repeated].name.clone());
        let (_, key_ty, key) = self.field(def + 1, rep + 1)?;
        let (_, value_ty, value) = self.field(def + 1, rep + 1)?;
        self.path.pop();
        let shape =
            Shape::Map { def: def + 1, rep: rep + 1, key: Box::new(key), value: Box::new(value) };
        Ok((LogicalType::Map(Box::new(key_ty), Box::new(value_ty)), shape))
    }

    /// The one repeated child of a list or a map, stepped past.
    fn repeated(&mut self, group: &SchemaElement) -> Result<usize> {
        let child = self.next()?;
        if group.children != 1 || self.elements[child].repetition != REPEATED {
            return Err(Error::not_implemented(format!(
                "a parquet {} named {} that is not one repeated group",
                if is_map(group) { "map" } else { "list" },
                group.name
            )));
        }
        Ok(child)
    }
}

/// Whether a group is annotated as a list.
fn is_list(element: &SchemaElement) -> bool {
    matches!(element.logical, Some(Logical::List)) || element.converted == Some(3)
}

/// Whether a group is annotated as a map, which old writers spelled `MAP_KEY_VALUE`.
fn is_map(element: &SchemaElement) -> bool {
    matches!(element.logical, Some(Logical::Map)) || matches!(element.converted, Some(1 | 2))
}

/// One element of the schema list, with the annotations still separate.
#[derive(Debug, Default)]
struct SchemaElement {
    name: String,
    physical: Option<Physical>,
    width: i32,
    repetition: i64,
    children: i64,
    converted: Option<i64>,
    scale: i32,
    precision: i32,
    logical: Option<Logical>,
}

/// The `logicalType` union, as far as a flat reader cares about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Logical {
    /// `STRING`, which is UTF-8.
    String,
    /// `ENUM`, which is a string with a promise nobody checks.
    Enum,
    /// `JSON`, a string with a shape.
    Json,
    /// `BSON`, bytes with a shape.
    Bson,
    /// `UUID`, sixteen fixed bytes.
    Uuid,
    /// `DATE`, days since the epoch in an `INT32`.
    Date,
    /// `DECIMAL`, whose precision and scale are on the element itself as well.
    Decimal,
    /// `TIME`, at the unit the file states, and whether it is UTC.
    Time(Unit, bool),
    /// `TIMESTAMP`, at the unit the file states, and whether it is UTC.
    Timestamp(Unit, bool),
    /// `INTEGER`, with a width and a sign, which is how the newer files spell `UINT_32`.
    Integer(i8, bool),
    /// `LIST`, on a group.
    List,
    /// `MAP`, on a group.
    Map,
}

/// The resolution of a time or a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    /// Thousandths of a second.
    Millis,
    /// Millionths, which is what DuckDB's `TIMESTAMP` is.
    Micros,
    /// Billionths.
    Nanos,
}

/// Reads one `SchemaElement`.
fn read_schema_element(reader: &mut Reader<'_>) -> Result<SchemaElement> {
    let saved = reader.struct_begin();
    let mut element = SchemaElement::default();
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => element.physical = Some(Physical::from_wire(reader.read_int()?)?),
            2 => element.width = reader.read_int()? as i32,
            3 => element.repetition = reader.read_int()?,
            4 => element.name = reader.read_string()?.to_string(),
            5 => element.children = reader.read_int()?,
            6 => element.converted = Some(reader.read_int()?),
            7 => element.scale = reader.read_int()? as i32,
            8 => element.precision = reader.read_int()? as i32,
            10 => element.logical = read_logical_type(reader)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(element)
}

/// Reads the `logicalType` union, which is a structure with exactly one field set.
fn read_logical_type(reader: &mut Reader<'_>) -> Result<Option<Logical>> {
    let saved = reader.struct_begin();
    let mut logical = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::String);
            }
            2 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Map);
            }
            3 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::List);
            }
            4 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Enum);
            }
            5 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Decimal);
            }
            6 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Date);
            }
            7 => {
                let (unit, utc) = read_time(reader)?;
                logical = Some(Logical::Time(unit, utc));
            }
            8 => {
                let (unit, utc) = read_time(reader)?;
                logical = Some(Logical::Timestamp(unit, utc));
            }
            10 => logical = Some(read_integer(reader)?),
            12 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Json);
            }
            13 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Bson);
            }
            14 => {
                reader.skip(field.kind)?;
                logical = Some(Logical::Uuid);
            }
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(logical)
}

/// Reads a `TimeType` or a `TimestampType`, which have the same two fields.
fn read_time(reader: &mut Reader<'_>) -> Result<(Unit, bool)> {
    let saved = reader.struct_begin();
    let mut unit = Unit::Millis;
    let mut utc = false;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => utc = reader.read_bool(field.kind, false)?,
            2 => unit = read_unit(reader)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok((unit, utc))
}

/// Reads a `TimeUnit`, which is a union of three empty structures.
fn read_unit(reader: &mut Reader<'_>) -> Result<Unit> {
    let saved = reader.struct_begin();
    let mut unit = Unit::Millis;
    while let Some(field) = reader.field_begin()? {
        reader.skip(field.kind)?;
        unit = match field.id {
            1 => Unit::Millis,
            2 => Unit::Micros,
            3 => Unit::Nanos,
            other => return Err(Error::io(format!("a parquet time unit of {other}"))),
        };
    }
    reader.struct_end(saved);
    Ok(unit)
}

/// Reads an `IntType`, which is a width and a sign.
fn read_integer(reader: &mut Reader<'_>) -> Result<Logical> {
    let saved = reader.struct_begin();
    let mut width = 32_i8;
    let mut signed = true;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => width = reader.read_byte()?,
            2 => signed = reader.read_bool(field.kind, false)?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(Logical::Integer(width, signed))
}

/// What rudb calls a column, given how it is stored and what the file annotates it with.
///
/// The `logicalType` union wins over the older `converted_type` when both are there, which is what
/// the format says to do and what matters on the one case where they disagree: a `converted_type`
/// of `TIMESTAMP_MILLIS` on a file whose `logicalType` says nanoseconds.
fn logical_type(physical: Physical, element: &SchemaElement) -> Result<LogicalType> {
    if let Some(logical) = element.logical {
        return from_logical(physical, logical, element);
    }
    if let Some(converted) = element.converted {
        return from_converted(physical, converted, element);
    }
    Ok(match physical {
        Physical::Boolean => LogicalType::Boolean,
        Physical::Int32 => LogicalType::Integer,
        Physical::Int64 => LogicalType::BigInt,
        Physical::Int96 => LogicalType::TimestampNs,
        Physical::Float => LogicalType::Float,
        Physical::Double => LogicalType::Double,
        Physical::ByteArray | Physical::FixedLenByteArray => LogicalType::Blob,
    })
}

/// The mapping for the `logicalType` union.
fn from_logical(
    physical: Physical,
    logical: Logical,
    element: &SchemaElement,
) -> Result<LogicalType> {
    Ok(match logical {
        Logical::String | Logical::Enum | Logical::Json => LogicalType::Varchar,
        Logical::Bson => LogicalType::Blob,
        Logical::Uuid => LogicalType::Uuid,
        Logical::Date => LogicalType::Date,
        Logical::Decimal => decimal(element)?,
        // A time the file says is at UTC is how the pin writes a zoned time, and it reads one back
        // as a zoned time at offset zero, whatever its unit.
        Logical::Time(_, true) => LogicalType::TimeTz,
        Logical::Time(Unit::Nanos, false) => LogicalType::TimeNs,
        Logical::Time(..) => LogicalType::Time,
        Logical::Timestamp(unit, utc) => timestamp(unit, utc),
        Logical::Integer(width, signed) => integer(width, signed, physical)?,
        Logical::List | Logical::Map => {
            return Err(Error::io(format!(
                "a leaf column named {} annotated as a list or a map",
                element.name
            )));
        }
    })
}

/// The mapping for the older `converted_type` enumeration.
fn from_converted(
    physical: Physical,
    converted: i64,
    element: &SchemaElement,
) -> Result<LogicalType> {
    Ok(match converted {
        0 | 24 => LogicalType::Varchar,
        4 => LogicalType::Varchar,
        5 => decimal(element)?,
        6 => LogicalType::Date,
        7 | 8 => LogicalType::Time,
        9 => LogicalType::Timestamp,
        10 => LogicalType::Timestamp,
        11 => LogicalType::UTinyInt,
        12 => LogicalType::USmallInt,
        13 => LogicalType::UInteger,
        14 => LogicalType::UBigInt,
        15 => LogicalType::TinyInt,
        16 => LogicalType::SmallInt,
        17 => LogicalType::Integer,
        18 => LogicalType::BigInt,
        20 => LogicalType::Blob,
        // `MAP`, `LIST`, `MAP_KEY_VALUE` and `INTERVAL` all land here, and every one of them is
        // either nested, which the schema reader has already refused, or a type rudb does not have
        // yet. Falling back on the physical type would read the bytes and call them something they
        // are not, so this says so instead.
        other => {
            return Err(Error::not_implemented(format!(
                "a parquet converted type of {other} on a {physical:?} column"
            )));
        }
    })
}

/// A `DECIMAL`, whose precision and scale are on the schema element rather than on the annotation.
fn decimal(element: &SchemaElement) -> Result<LogicalType> {
    let width = u8::try_from(element.precision).map_err(|_| {
        Error::io(format!("a decimal of precision {}, which is not one", element.precision))
    })?;
    let scale = u8::try_from(element.scale).map_err(|_| {
        Error::io(format!("a decimal of scale {}, which is not one", element.scale))
    })?;
    LogicalType::decimal(width, scale)
}

/// A timestamp of the file's unit, which is a `TIMESTAMP` for milliseconds as well as for
/// microseconds, the way the pin reads one.
///
/// A file that says its timestamps are UTC gets the type with a zone, which is what DuckDB does
/// with the same file, and that matters because the two types print differently.
fn timestamp(unit: Unit, utc: bool) -> LogicalType {
    match (unit, utc) {
        (Unit::Nanos, false) => LogicalType::TimestampNs,
        (Unit::Nanos, true) => LogicalType::TimestampTzNs,
        (_, false) => LogicalType::Timestamp,
        (_, true) => LogicalType::TimestampTz,
    }
}

/// The unit a time or a timestamp column is stored at, from its logical type or else its converted
/// one.
fn unit(element: &SchemaElement) -> Option<Unit> {
    match element.logical {
        Some(Logical::Time(unit, _) | Logical::Timestamp(unit, _)) => Some(unit),
        Some(_) => None,
        None => match element.converted? {
            7 | 9 => Some(Unit::Millis),
            8 | 10 => Some(Unit::Micros),
            _ => None,
        },
    }
}

/// An integer of the width and sign the annotation states.
fn integer(width: i8, signed: bool, physical: Physical) -> Result<LogicalType> {
    Ok(match (width, signed) {
        (8, true) => LogicalType::TinyInt,
        (16, true) => LogicalType::SmallInt,
        (32, true) => LogicalType::Integer,
        (64, true) => LogicalType::BigInt,
        (8, false) => LogicalType::UTinyInt,
        (16, false) => LogicalType::USmallInt,
        (32, false) => LogicalType::UInteger,
        (64, false) => LogicalType::UBigInt,
        _ => {
            return Err(Error::io(format!(
                "a parquet integer annotation of {width} bits on a {physical:?} column"
            )));
        }
    })
}

/// Reads one `RowGroup`.
fn read_row_group(reader: &mut Reader<'_>) -> Result<RawGroup> {
    let saved = reader.struct_begin();
    let mut group = RawGroup { columns: Vec::new(), rows: 0, bytes: 0 };
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => {
                let (len, kind) = reader.list_begin()?;
                expect(kind, Kind::Struct, "column chunks")?;
                group.columns.reserve(len);
                for _ in 0..len {
                    group.columns.push(read_column_chunk(reader)?);
                }
            }
            2 => group.bytes = reader.read_int()?,
            3 => group.rows = reader.read_int()?,
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(group)
}

/// Reads one `ColumnChunk`, which is a wrapper whose third field is what a reader wants.
fn read_column_chunk(reader: &mut Reader<'_>) -> Result<RawChunk> {
    let saved = reader.struct_begin();
    let mut chunk = None;
    let (mut pages_at, mut pages_len) = (None, None);
    let (mut index_at, mut index_len) = (None, None);
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => {
                let path = reader.read_string()?;
                if !path.is_empty() {
                    return Err(Error::not_implemented(format!(
                        "a column chunk in another file, {path}"
                    )));
                }
            }
            3 => chunk = Some(read_column_metadata(reader)?),
            4 => pages_at = Some(offset(reader.read_int()?, "an offset index")?),
            5 => pages_len = Some(reader.read_int()?),
            6 => index_at = Some(offset(reader.read_int()?, "a column index")?),
            7 => index_len = Some(reader.read_int()?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    let mut chunk = chunk.ok_or_else(|| Error::io("a column chunk with no metadata in it"))?;
    chunk.offset_index = region(pages_at, pages_len);
    chunk.column_index = region(index_at, index_len);
    Ok(chunk)
}

/// A region from the offset and the length the footer carries separately.
///
/// Both or neither. A writer that recorded one and not the other has said nothing a reader can act
/// on, and treating half a pair as an error would fail a file that is otherwise readable.
fn region(at: Option<u64>, len: Option<i64>) -> Option<Region> {
    let (at, len) = (at?, usize::try_from(len?).ok()?);
    (len > 0).then_some(Region { at, len })
}

/// Reads one `ColumnMetaData`, which is where a column chunk's offsets live.
fn read_column_metadata(reader: &mut Reader<'_>) -> Result<RawChunk> {
    let saved = reader.struct_begin();
    let mut physical = Physical::Boolean;
    let mut path = Vec::new();
    let mut compression = Codec::Uncompressed;
    let mut encodings = Vec::new();
    let mut values = 0;
    let mut uncompressed_size = 0;
    let mut compressed_size = 0;
    let mut data_page_offset = 0;
    let mut dictionary_page_offset = None;
    let mut stats = None;
    while let Some(field) = reader.field_begin()? {
        match field.id {
            1 => physical = Physical::from_wire(reader.read_int()?)?,
            2 => {
                let (len, kind) = reader.list_begin()?;
                expect(kind, Kind::I32, "encodings")?;
                for _ in 0..len {
                    encodings.push(Encoding::from_wire(reader.read_int()?)?);
                }
            }
            3 => {
                let (len, kind) = reader.list_begin()?;
                expect(kind, Kind::Binary, "a column path")?;
                for _ in 0..len {
                    path.push(reader.read_string()?.to_string());
                }
            }
            4 => compression = codec(reader.read_int()?)?,
            5 => values = reader.read_int()?,
            6 => uncompressed_size = reader.read_int()?,
            7 => compressed_size = reader.read_int()?,
            9 => data_page_offset = offset(reader.read_int()?, "a data page")?,
            11 => dictionary_page_offset = Some(offset(reader.read_int()?, "a dictionary page")?),
            12 => stats = Some(read_stats(reader)?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(RawChunk {
        path,
        physical,
        compression,
        encodings,
        values,
        compressed_size,
        uncompressed_size,
        data_page_offset,
        dictionary_page_offset,
        stats,
        column_index: None,
        offset_index: None,
    })
}

/// Reads a `Statistics`, taking the bounds that have a defined ordering and leaving the others.
///
/// Shared with the page headers, which carry the same structure per page where the writer emitted
/// it, so the two cannot disagree about which bounds are safe to trust.
pub(crate) fn read_stats(reader: &mut Reader<'_>) -> Result<Stats> {
    let saved = reader.struct_begin();
    let mut stats = Stats::default();
    while let Some(field) = reader.field_begin()? {
        match field.id {
            3 => stats.nulls = Some(reader.read_int()?),
            4 => stats.distinct = Some(reader.read_int()?),
            5 => stats.max = Some(reader.read_binary()?.to_vec()),
            6 => stats.min = Some(reader.read_binary()?.to_vec()),
            7 => stats.max_exact = Some(reader.read_bool(field.kind, false)?),
            8 => stats.min_exact = Some(reader.read_bool(field.kind, false)?),
            _ => reader.skip(field.kind)?,
        }
    }
    reader.struct_end(saved);
    Ok(stats)
}

/// The codec with this wire value.
///
/// `rudb_compress::Codec` rather than a second enum here, because a codec read out of a footer
/// and a codec handed to a decompressor are the same thing, and two enums that have to agree is
/// one more than the number worth having.
fn codec(wire: i64) -> Result<Codec> {
    let code = i32::try_from(wire)
        .map_err(|_| Error::io(format!("a parquet codec of {wire}, which is not one")))?;
    Codec::from_parquet(code)
}

/// A file offset, which the footer states as a signed integer and which cannot be negative.
fn offset(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::io(format!("{what} at offset {value}")))
}

/// Checks that a container holds what the structure says it holds.
fn expect(found: Kind, wanted: Kind, what: &str) -> Result<()> {
    if found == wanted {
        return Ok(());
    }
    Err(Error::io(format!("{what} written as a list of {found:?} rather than {wanted:?}")))
}

/// The footer bytes of an open file, checked the way [`Metadata::read`] says.
fn footer(file: &dyn File) -> Result<Vec<u8>> {
    let (at, length) = footer_span(file)?;
    let mut bytes = vec![0_u8; length];
    file.read_exact_at(at, &mut bytes)?;
    Ok(bytes)
}

/// Where a file's footer starts and how long it is, after checking both ends say Parquet.
fn footer_span(file: &dyn File) -> Result<(u64, usize)> {
    let len = file.len()?;
    if len < 12 {
        return Err(Error::io(format!("a parquet file of {len} bytes, which is too short")));
    }
    let mut head = [0_u8; 4];
    file.read_exact_at(0, &mut head)?;
    if &head != MAGIC {
        return Err(Error::io("a file that does not start with PAR1"));
    }
    let mut tail = [0_u8; 8];
    file.read_exact_at(len - 8, &mut tail)?;
    if &tail[4..] != MAGIC {
        return Err(Error::io("a file that does not end with PAR1"));
    }
    let footer = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    if footer + 8 > len {
        return Err(Error::io(format!(
            "a footer of {footer} bytes in a file of {len}, which does not fit"
        )));
    }
    let length = usize::try_from(footer).map_err(|_| Error::io("a footer too long to read"))?;
    Ok((len - 8 - footer, length))
}

/// The columns of a file and how many rows it holds, without its row groups.
///
/// What a caller that is only naming columns needs. A `CREATE VIEW` over a file binds its body to
/// learn the columns and throws the plan away, and a query that is about to be bound again through
/// a native mirror throws its first plan away too. Neither reads a bound or a distinct count. On
/// the ten million row ClickBench file the row groups are almost all of the footer, eighty two of
/// them with a hundred and five chunks each, and parsing them held two megabytes and peaked at
/// three, which was a quarter of what the process needed to answer `SELECT COUNT(*)` from a
/// mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// The columns, flat, in the order the file stores them.
    pub schema: Vec<SchemaColumn>,
    /// How many rows are in the file, across every row group.
    pub rows: i64,
}

/// How much of the front of a footer [`Outline::read`] reads before it gives up and reads it all.
const OUTLINE_FRONT: usize = 64 * 1024;

impl Outline {
    /// Reads the outline of an open file, which reads the front of the footer and parses part of it.
    ///
    /// The schema and the row count are the second and third fields of the footer and the row
    /// groups come after them, so the front of it is usually all there is to read. On the ten
    /// million row ClickBench file the footer is 930 KB and the outline is inside its first 64 KB.
    /// Reading the whole of it put a freed megabyte in the process for every statement that named
    /// the file, which the allocator keeps, and a statement read through a mirror names it twice.
    /// A front that ends before both fields have been seen falls back to reading the whole footer.
    ///
    /// # Errors
    ///
    /// Everything [`Metadata::read`] reports, less what it finds wrong with a row group.
    pub fn read(file: &dyn File) -> Result<Self> {
        let (at, length) = footer_span(file)?;
        if length > OUTLINE_FRONT {
            let mut front = vec![0_u8; OUTLINE_FRONT];
            file.read_exact_at(at, &mut front)?;
            if let Ok(Some(outline)) = Self::parse_front(&front) {
                return Ok(outline);
            }
        }
        let mut bytes = vec![0_u8; length];
        file.read_exact_at(at, &mut bytes)?;
        Self::parse(&bytes)
    }

    /// The outline out of the front of a footer, or `None` when the front ends before it does.
    ///
    /// The front is cut wherever the read stopped, so running out of it is an error from the
    /// reader, and the caller takes any error here as a reason to read the whole footer. That
    /// read is then the one that reports what is wrong with the file.
    fn parse_front(bytes: &[u8]) -> Result<Option<Self>> {
        let mut reader = Reader::new(bytes);
        let mut rows = None;
        let mut schema = Vec::new();
        while let Some(field) = reader.field_begin()? {
            match field.id {
                2 => schema = read_schema(&mut reader)?,
                3 => rows = Some(reader.read_int()?),
                _ => reader.skip(field.kind)?,
            }
            if let (false, Some(rows)) = (schema.is_empty(), rows) {
                return Ok(Some(Self { schema, rows }));
            }
        }
        Ok(None)
    }

    /// Parses the outline out of a footer that has already been read.
    ///
    /// # Errors
    ///
    /// If the bytes are not a `FileMetaData` or the schema is nested.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let mut rows = 0;
        let mut schema = Vec::new();
        while let Some(field) = reader.field_begin()? {
            match field.id {
                2 => schema = read_schema(&mut reader)?,
                3 => rows = reader.read_int()?,
                _ => reader.skip(field.kind)?,
            }
        }
        if schema.is_empty() {
            return Err(Error::io("a parquet footer with no schema in it"));
        }
        Ok(Self { schema, rows })
    }

    /// The columns as rudb sees them, the way [`crate::Reader::fields`] reads them.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.schema
            .iter()
            .map(|column| {
                if column.optional {
                    Field::new(column.name.clone(), column.ty.clone())
                } else {
                    Field::required(column.name.clone(), column.ty.clone())
                }
            })
            .collect()
    }

    /// How many rows the file holds, where the footer states a count that is not negative.
    #[must_use]
    pub fn rows(&self) -> Option<u64> {
        u64::try_from(self.rows).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rudb_common::LogicalType;
    use rudb_io::{Filesystem, OpenMode, RealFilesystem};

    use super::{Codec, Encoding, Metadata, Outline, Physical};

    /// The file every test here reads.
    ///
    /// It is 37 kilobytes written by DuckDB with Snappy and two row groups of 2048 rows, and it has
    /// one column of every shape the reader has to tell apart: plain and dictionary encoded, a
    /// column with nulls in it and columns without, and a date and a timestamp, which are the two
    /// annotations that turn an integer into something else. Files from the other writers come in
    /// with the page decoders, because a writer's disagreements are about pages and not about the
    /// footer.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/mixed.parquet")
    }

    /// The fixture's footer, read through the real filesystem the way a scan would read it.
    fn read() -> Metadata {
        let fs = RealFilesystem::new();
        let file = fs.open(&fixture(), OpenMode::Read).expect("opens the fixture");
        Metadata::read(file.as_ref()).expect("reads the footer")
    }

    #[test]
    fn the_distinct_counts_the_writer_stated_come_back_and_the_ones_it_did_not_are_absent() {
        // DuckDB states this for the columns it already knew the number for and leaves it off the
        // rest, so a reader has to carry the difference rather than filling one in. The two row
        // groups agree here because both hold the same values.
        let metadata = read();
        for group in &metadata.row_groups {
            let stated: Vec<Option<i64>> = group
                .columns
                .iter()
                .map(|chunk| chunk.stats.as_ref().and_then(|stats| stats.distinct))
                .collect();
            // a, b, s, d, flag, day, t.
            assert_eq!(stated, vec![Some(97), None, Some(5), Some(64), None, None, None]);
        }
    }

    #[test]
    fn an_outline_is_the_footer_without_its_row_groups() {
        let fs = RealFilesystem::new();
        let file = fs.open(&fixture(), OpenMode::Read).expect("opens the fixture");
        let outline = Outline::read(file.as_ref()).expect("reads the outline");
        let metadata = read();
        assert_eq!(outline.schema, metadata.schema, "the same columns");
        assert_eq!(outline.rows, metadata.rows, "the same row count");
        let reader = crate::Reader::open(fs.open(&fixture(), OpenMode::Read).expect("opens"))
            .expect("reads the footer");
        assert_eq!(outline.fields(), reader.fields(), "the same fields a reader names");
    }

    #[test]
    fn the_schema_comes_back_as_duckdb_wrote_it() {
        let metadata = read();
        let names: Vec<&str> = metadata.schema.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "s", "d", "flag", "day", "t"]);
        let types: Vec<LogicalType> = metadata.schema.iter().map(|c| c.ty.clone()).collect();
        assert_eq!(
            types,
            vec![
                LogicalType::Integer,
                LogicalType::BigInt,
                LogicalType::Varchar,
                LogicalType::Double,
                LogicalType::Boolean,
                LogicalType::Date,
                LogicalType::Timestamp,
            ]
        );
        let physical: Vec<Physical> = metadata.schema.iter().map(|c| c.physical).collect();
        assert_eq!(
            physical,
            vec![
                Physical::Int32,
                Physical::Int64,
                Physical::ByteArray,
                Physical::Double,
                Physical::Boolean,
                Physical::Int32,
                Physical::Int64,
            ]
        );
        assert!(metadata.schema.iter().all(|column| column.optional));
    }

    #[test]
    fn the_row_groups_say_where_every_column_is() {
        let metadata = read();
        assert_eq!(metadata.rows, 4096);
        assert_eq!(metadata.row_groups.len(), 2);
        for group in &metadata.row_groups {
            assert_eq!(group.rows, 2048);
            assert_eq!(group.columns.len(), 7);
            for (at, chunk) in group.columns.iter().enumerate() {
                assert_eq!(chunk.column, at);
                assert_eq!(chunk.values, 2048);
                assert_eq!(chunk.compression, Codec::Snappy);
                assert!(chunk.data_page_offset > 0, "a data page at offset zero");
            }
        }
        // The second group starts after the first one ends, which is the property a reader that
        // seeks straight to a column chunk depends on.
        let first = &metadata.row_groups[0].columns[6];
        let second = &metadata.row_groups[1].columns[0];
        assert!(second.start() > first.start());
    }

    #[test]
    fn a_dictionary_column_says_so_and_a_plain_one_does_not() {
        let metadata = read();
        let group = &metadata.row_groups[0];
        let dictionary = &group.columns[metadata.column_named("s").expect("has s")];
        assert!(dictionary.encodings.iter().any(|e| e.is_dictionary()));
        assert!(dictionary.dictionary_page_offset.is_some());
        assert_eq!(
            dictionary.start(),
            dictionary.dictionary_page_offset.expect("has a dictionary")
        );
        assert!(dictionary.encodings.contains(&Encoding::RleDictionary));
        let plain = &group.columns[metadata.column_named("b").expect("has b")];
        assert!(plain.dictionary_page_offset.is_none());
        assert_eq!(plain.start(), plain.data_page_offset);
        assert!(plain.encodings.contains(&Encoding::Plain));
    }

    #[test]
    fn the_writer_counted_the_nulls_and_the_count_is_the_one_in_the_data() {
        let metadata = read();
        let group = &metadata.row_groups[0];
        let with_nulls = &group.columns[metadata.column_named("s").expect("has s")];
        let stats = with_nulls.stats.as_ref().expect("the writer wrote statistics");
        // Every seventh row of the first 2048 is null, which is 293 of them.
        assert_eq!(stats.nulls, Some(293));
        assert!(stats.min.is_some() && stats.max.is_some());
        let without = &group.columns[metadata.column_named("a").expect("has a")];
        assert_eq!(without.stats.as_ref().expect("statistics").nulls, Some(0));
    }

    #[test]
    fn a_column_is_found_by_name_the_way_sql_finds_it() {
        let metadata = read();
        assert_eq!(metadata.column_named("flag"), Some(4));
        assert_eq!(metadata.column_named("FLAG"), Some(4));
        assert_eq!(metadata.column_named("nothing"), None);
    }

    #[test]
    fn the_description_names_every_column_once() {
        let metadata = read();
        let text = metadata.describe();
        assert!(text.starts_with("4096 rows in 2 row groups, written by "));
        assert_eq!(text.lines().count(), 1 + metadata.schema.len());
        for (at, column) in metadata.schema.iter().enumerate() {
            let wanted = format!("{at} {} {} optional", column.name, column.ty);
            assert!(text.lines().any(|line| line == wanted), "{wanted} is not in the description");
        }
    }

    #[test]
    fn a_file_that_is_not_parquet_is_refused_rather_than_parsed() {
        let fs = RealFilesystem::new();
        let scratch = std::env::temp_dir().join("rudb-parquet-not-parquet");
        let file = fs.open(&scratch, OpenMode::Create).expect("opens a scratch file");
        file.write_at(0, b"this is not a parquet file at all").expect("writes");
        file.sync().expect("syncs");
        let error = Metadata::read(file.as_ref()).expect_err("refuses a file that is not parquet");
        assert!(error.message().contains("PAR1"), "{}", error.message());
        drop(file);
        fs.remove(&scratch).expect("cleans up");
    }

    #[test]
    fn a_footer_longer_than_the_file_is_refused() {
        let bytes = read_fixture_bytes();
        let mut broken = bytes.clone();
        let len = broken.len();
        broken[len - 8..len - 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let fs = RealFilesystem::new();
        let scratch = std::env::temp_dir().join("rudb-parquet-long-footer");
        let file = fs.open(&scratch, OpenMode::Create).expect("opens a scratch file");
        file.write_at(0, &broken).expect("writes");
        file.sync().expect("syncs");
        let error = Metadata::read(file.as_ref()).expect_err("refuses a footer that does not fit");
        assert!(error.message().contains("does not fit"), "{}", error.message());
        drop(file);
        fs.remove(&scratch).expect("cleans up");
    }

    #[test]
    fn a_truncated_footer_is_an_error_and_never_a_panic() {
        let bytes = read_fixture_bytes();
        let len = bytes.len();
        let footer =
            u32::from_le_bytes([bytes[len - 8], bytes[len - 7], bytes[len - 6], bytes[len - 5]])
                as usize;
        let start = len - 8 - footer;
        // Every prefix of the footer, at a stride that keeps the test quick, has to come back as an
        // error. A footer is a structure of nested structures and a reader that trusted a length in
        // it would read past the end of the buffer on almost every one of these.
        for cut in (1..footer).step_by(7) {
            let outcome = Metadata::parse(&bytes[start..start + cut]);
            assert!(outcome.is_err(), "a footer cut to {cut} bytes parsed");
        }
        assert!(Metadata::parse(&bytes[start..start + footer]).is_ok());
    }

    /// The whole fixture as bytes, which the corruption tests edit.
    fn read_fixture_bytes() -> Vec<u8> {
        let fs = RealFilesystem::new();
        let file = fs.open(&fixture(), OpenMode::Read).expect("opens the fixture");
        let len = file.len().expect("has a length") as usize;
        let mut bytes = vec![0_u8; len];
        file.read_exact_at(0, &mut bytes).expect("reads the whole file");
        bytes
    }
}
