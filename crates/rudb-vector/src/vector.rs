//! The vector itself.
//!
//! `spec/07-execution.md` section 7.1 calls this the widest interface in the system, says every
//! operator depends on it, and says changing it after twenty operators exist is expensive. So it
//! is written before the first operator rather than after the fifth.
//!
//! A vector is a type, a length of at most [`VECTOR_SIZE`], a physical form, a validity
//! representation and some data. Four of the forms are the ones in `spec/04-architecture.md`
//! section 4.3: flat, constant, sequence and dictionary. Run length, bit packed and string view come
//! after them, one at a time with the kernels that read them rather than all at once ahead of
//! anything that can use them.
//!
//! Dictionary and run length are the pair worth understanding together, because they answer
//! different questions about the same column. A dictionary says which distinct values there are, so
//! it wins on low cardinality however the rows are ordered. Run length says where the values stop,
//! so it wins on a clustered column however many distinct values it has. A column can want either
//! one without wanting the other, and `hits` has columns of both kinds.
//!
//! String view is the odd one out, because it is not about making a column smaller. It is about who
//! owns the bytes: the views are the vector's and the arena is shared, so cutting a chunk out of a
//! page of strings moves sixteen bytes a row and copies none of the payload. Every other form here
//! trades a little work per row for less memory, and that one trades nothing at all.
//!
//! The nested forms are the odd ones out in a different direction. The forms above are all ways of
//! writing a column of scalars down more cheaply, and a nested value is not a scalar at all, so
//! [`Form::List`] and [`Form::Struct`] are each the only form their column has rather than one of
//! several it could be in. A list is a child vector of every element plus a start and a length per
//! row. A struct is one child per field with no entries at all, because a struct row holds one value
//! per field rather than a run of them. Either way the children are ordinary vectors and can be in any
//! of the forms above, which is where a nested column gets made smaller.
//!
//! **What is not here yet.** Buffers are owned. Section 7.1 says a vector borrowed from a buffer
//! managed page carries a pin, and there is no buffer manager until M2, so there is nothing to pin
//! and pretending otherwise would be an interface built against an imaginary caller. `ARRAY` is not
//! stored yet either, and it is a composition of what is here rather than a new shape: it is a list
//! whose length is the type's rather than the row's, the way a `MAP` is a list whose child is a two
//! field struct of keys and values. `UNION` is the one that is genuinely different, since it is one
//! child per member plus a tag saying which member each row is in.

use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::sync::Arc;

use rudb_common::{Cause, Error, Field, LogicalType, Result, Value, slow};

use crate::buffer::Buffer;
use crate::fsst::SymbolTable;
use crate::string::{StringColumn, StringView};
use crate::validity::Validity;

/// How many values are in a full vector.
///
/// 8192, which is four times DuckDB's 2048 and eight times what this was. It started at 1024 for
/// three reasons: the FastLanes unit is 1024, a validity mask comes out at exactly 16 `u64` words,
/// and a vector of 16 byte string views is 16 KiB, which is small enough that several of them sit
/// in L1 at once. The first two are still true of any multiple of 1024. The third was the argument
/// and it was an argument about the wrong level, because it was also deciding how much of a table
/// one zone map covered and how much work one call into the pipeline did, and those wanted a much
/// larger number than L1 did.
///
/// #984 separated them: a table in memory is stored in row groups of 122,880 rows now and a chunk
/// is a window into one, so the vector size is only the execution unit and is free to be chosen for
/// what an operator costs per call. #480 measured it. On twenty million rows in memory, one thread,
/// going from 1024 to 8192 takes `count(*)` with a filter from 14.0 milliseconds to 1.9, `sum(v)`
/// with the same filter from 39.6 to 29.6 and `sum(k + v)` from 66.8 to 52.6. On ClickBench over
/// Parquet, where the time is decode and hash aggregation rather than per call overhead, the same
/// move is worth about eight percent on the total of the twenty nine queries that run.
///
/// 32768 was measured too and is not better: it wins another few percent on the full scans and
/// loses on the load, on a needle that the chunk zone maps would otherwise prune, and on anything
/// with a string column, where a vector of views is half a megabyte. 8192 is where the per call
/// overhead has stopped mattering and the working set has not started to.
pub const VECTOR_SIZE: usize = 8192;

/// The smallest and largest of `at`, or `None` when it is empty.
///
/// Compared as signed 32 bit numbers with the top bit flipped, which keeps the order and is the
/// one minimum and maximum SSE2 has, so the loop vectorizes where an unsigned one does not.
fn extent(at: &[u32]) -> Option<(u32, u32)> {
    const FLIP: u32 = 1 << 31;
    #[expect(clippy::cast_possible_wrap, reason = "the flip makes the wrap keep the order")]
    let signed = |row: u32| (row ^ FLIP) as i32;
    #[expect(clippy::cast_sign_loss, reason = "undoing the flip above")]
    let unsigned = |row: i32| (row as u32) ^ FLIP;
    if at.is_empty() {
        return None;
    }
    let low = at.iter().fold(i32::MAX, |low, &row| low.min(signed(row)));
    let high = at.iter().fold(i32::MIN, |high, &row| high.max(signed(row)));
    Some((unsigned(low), unsigned(high)))
}

/// Whether every one of `codes` is below `len`.
///
/// The obvious test is the largest code, and on the baseline x86-64 the release is built for that
/// loop does not vectorize, because SSE2 has no unsigned 32 bit maximum. It was about half of
/// `Vector::gather` on q01, where every filtered column asks it of the same positions. An `or` of
/// every code is at least as large as each of them and does vectorize, so when it is below `len`
/// every code is too. A filter's positions over a full chunk of 8192 rows always pass that way,
/// since `len` is then a power of two. Anything the `or` cannot settle takes the maximum.
pub(crate) fn below(codes: &[u32], len: usize) -> bool {
    let Ok(len) = u32::try_from(len) else { return true };
    if codes.is_empty() || codes.iter().fold(0, |bits, &code| bits | code) < len {
        return true;
    }
    codes.iter().copied().fold(0, u32::max) < len
}

/// What the key field of a map's child struct is called.
///
/// A map is stored as a list of two field structs, and these are the two names. They are DuckDB's, and
/// they are also the names the Parquet specification gives a map's repeated group, so a reader that
/// builds one of these from a file finds the names already agreed rather than translated.
pub const MAP_KEY: &str = "key";

/// What the value field of a map's child struct is called. See [`MAP_KEY`].
pub const MAP_VALUE: &str = "value";

/// What [`Vector::map_parts`] hands back: one entry per row, then the keys and then the values.
///
/// A name rather than the triple written out, because the triple written out is over the complexity
/// clippy allows and because a kernel that takes these as an argument should be able to say so in one
/// word.
pub type MapParts<'a> = (&'a [(u32, u32)], &'a Vector, &'a Vector);

/// Which physical form a vector is in.
///
/// An operator asks this once per vector and then takes the path it wants, which is the one branch
/// per vector that the whole design is willing to spend.
///
/// Not exhaustive, and that is a decision rather than an oversight. `Encoded` is the fifth form
/// and it arrives at layer three with the specialization contract. If this enum were exhaustive,
/// the day it lands is the day every kernel in the workspace stops compiling, and the pressure at
/// that moment would be to add an arm to each of them in a hurry rather than to think about what
/// each one should do with an encoded vector. A required fallback arm means each kernel already
/// has a correct answer for a form it has never seen, and specializing it is then a change that
/// can be made one kernel at a time with a benchmark next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Form {
    /// One value per position.
    Flat,
    /// One value, repeated.
    Constant,
    /// A start and a step, computed rather than stored.
    Sequence,
    /// Codes into a smaller vector of distinct values.
    Dictionary,
    /// Integers stored in as many bits as the range of the column needs, offset from a base.
    ///
    /// The form a narrow integer column is in. A ClickBench `ResolutionWidth` is a `SMALLINT` whose
    /// values live between 0 and 2560, which is twelve bits, so the column is three quarters of the
    /// size it was and the pages behind it are three quarters of the reads. What it costs is a shift
    /// and a mask per value, which is why this is worth it at storage and at rest and is not a form
    /// anything should be building in the middle of a pipeline.
    BitPacked,
    /// Sixteen byte views over an arena the vector shares rather than owns.
    ///
    /// The form a varchar column is in once more than one vector is looking at the same page. A flat
    /// varchar vector owns its arena, so cutting a chunk out of it copies every byte of every long
    /// string in the range, and on ClickBench that is most of what reading `URL` costs. Sharing the
    /// arena makes the cut the views and nothing else, the way a dictionary cut is the codes and
    /// nothing else.
    StringView,
    /// Strings compressed against one symbol table, each row on its own.
    ///
    /// The form a text column is in at rest. FSST is about half the bytes on the ClickBench `URL`
    /// and `Title` columns, and unlike a block compressor it keeps random access, so reading row
    /// four million does not decompress the four million before it. What it costs is a decompression
    /// per row read, which is why an equality filter over it is worth writing in code space: the
    /// literal compresses once and the rows never decompress at all.
    Fsst,
    /// One value per run, with the row each run ends at.
    ///
    /// The form a clustered column is in. `hits` is written in time order, so `EventDate` is a few
    /// hundred runs over a hundred million rows, and a sum over it is a few hundred multiplications
    /// rather than a hundred million additions. Dictionary says which distinct values there are and
    /// this says where they stop, and a column can want either one without wanting the other.
    Rle,
    /// A child vector of every element, and a start and a length per row.
    ///
    /// The form a `LIST` column is in, and the only form it has. The others are all ways of writing
    /// down a column of scalars more cheaply and this is the shape a nested value has at all, so a
    /// list vector reports this whether or not anything has tried to make it smaller. Making it
    /// smaller happens in the child, which is an ordinary vector and can be any of the forms above.
    ///
    /// A `MAP` column reports this too, because a map is a list whose child is a two field struct and
    /// the bytes really are a list's. This enum is about the physical layout, and the logical type is
    /// what remembers the difference, which is the same division `LogicalType::physical` already makes.
    List,
    /// One child vector per field, each as long as the vector itself.
    ///
    /// The form a `STRUCT` column is in, and the only form it has, for the reason [`Form::List`] is
    /// the only form a list has. A struct holds exactly one value per field per row rather than a run
    /// of them, so there are no entries here and the children line up with the rows one to one, which
    /// makes a cut a cut of every child and a gather a gather of every child. Each child is an
    /// ordinary vector and can be in any of the forms above, so that is where a struct column gets
    /// made smaller.
    Struct,
    /// One row id per row, into a source vector that is far longer than this one.
    ///
    /// The form a link join's parent columns are in, per `spec/graph/08-vector-engine.md` section
    /// 8.2. Physically it is [`Form::Dictionary`] and logically it is the opposite of one, which is
    /// why it is a form of its own rather than a dictionary with a note on it. A dictionary promises
    /// that the values are few and distinct, and every kernel that has a dictionary arm takes that
    /// promise by folding the operation over the values once and then indexing. A gather's source is
    /// a whole parent table, so folding over it to answer two thousand rows reads fifteen million
    /// values for nothing. Both forms want the same code and they want it under opposite conditions,
    /// so the condition is [`Vector::fold_over_source`] and the form is what makes a kernel ask.
    Gathered,
}

/// The values of a flat vector, one Rust vector per physical type.
///
/// The variants are physical rather than logical, which is what lets `DATE` and `INTEGER` share
/// storage and share a kernel. What a run of `i32` means is the vector's logical type's business.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Data {
    /// No values, for the type of an untyped `NULL`.
    Empty,
    /// One byte per value.
    Bool(Buffer<bool>),
    /// 8 bit signed.
    Int8(Buffer<i8>),
    /// 16 bit signed.
    Int16(Buffer<i16>),
    /// 32 bit signed.
    Int32(Buffer<i32>),
    /// 64 bit signed.
    Int64(Buffer<i64>),
    /// 128 bit signed.
    Int128(Buffer<i128>),
    /// 8 bit unsigned.
    UInt8(Buffer<u8>),
    /// 16 bit unsigned.
    UInt16(Buffer<u16>),
    /// 32 bit unsigned.
    UInt32(Buffer<u32>),
    /// 64 bit unsigned.
    UInt64(Buffer<u64>),
    /// 128 bit unsigned.
    UInt128(Buffer<u128>),
    /// IEEE 754 binary32.
    Float32(Buffer<f32>),
    /// IEEE 754 binary64.
    Float64(Buffer<f64>),
    /// The months, days and microseconds triple.
    Interval(Buffer<(i32, i32, i64)>),
    /// Strings, as 16 byte views plus the arena the long ones live in.
    Varlen(StringColumn),
}

impl Data {
    /// How many values are stored.
    ///
    /// The match below has no wildcard arm, and that is what makes this function the check that
    /// keeps [`for_each_layout`](crate::for_each_layout) honest. A variant added to this enum
    /// without being added to the `all` group fails to compile here, which is a line in a build log
    /// rather than a layout quietly missing from six kernels.
    #[must_use]
    pub fn len(&self) -> usize {
        macro_rules! lengths {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match self {
                    Self::Empty => 0,
                    $(Self::$variant(values) => values.len(),)+
                }
            };
        }
        crate::for_each_layout!(all, lengths)
    }

    /// Whether there are no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many bytes of memory these values are holding.
    ///
    /// One arm per layout through the same macro as [`Data::len`], for the same reason: a layout
    /// added without a size here is a layout the memory limit would charge nothing for, and a
    /// buffer that is free is a buffer that can be grown until the process dies.
    #[must_use]
    pub fn footprint(&self) -> usize {
        macro_rules! sizes {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match self {
                    Self::Empty => 0,
                    $(Self::$variant(values) => values.footprint(),)+
                }
            };
        }
        crate::for_each_layout!(all, sizes)
    }

    /// These values held as a page, so that copying or cutting them does not copy the values.
    ///
    /// For a producer that is going to hand the same values out many times, which is what a stored
    /// column is. It costs one `Arc` per layout and moves the run into it without touching a value,
    /// and after it a write through any reader copies out rather than writing the page, which is
    /// [`Buffer::to_mut`]. A run that is already a page comes back as it was.
    #[must_use]
    pub fn into_pages(self) -> Self {
        macro_rules! paged {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match self {
                    Self::Empty => Self::Empty,
                    $(Self::$variant(values) => Self::$variant(values.into_page()),)+
                }
            };
        }
        crate::for_each_layout!(all, paged)
    }

    /// An integer at `index`, widened, for any of the signed integer layouts.
    ///
    /// Used by the decimal path, which needs the unscaled value out of whichever width the width
    /// and scale picked, and by anything else that would otherwise repeat the same five arms.
    #[must_use]
    pub fn signed_at(&self, index: usize) -> Option<i128> {
        macro_rules! widened {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match self {
                    $(Self::$variant(v) => v.get(index).map(|&x| i128::from(x)),)+
                    _ => None,
                }
            };
        }
        crate::for_each_layout!(signed, widened)
    }

    /// The first `len` signed integers, widened to `i64`, appended to `out`.
    ///
    /// The bulk form of [`Self::signed_at`]. Four of the five signed layouts, because the fifth is
    /// 128 bits wide and does not fit what this hands back. `Int64` is a copy of the run and the
    /// three narrower ones are a sign extension the compiler turns into one instruction per lane.
    ///
    /// `false`, leaving `out` as it found it, for the wide layout, for a run shorter than `len` and
    /// for every layout that is not a signed integer.
    #[must_use]
    pub fn signed_block(&self, len: usize, out: &mut Vec<i64>) -> bool {
        match self {
            Self::Int8(v) => widen(v.as_slice(), len, out),
            Self::Int16(v) => widen(v.as_slice(), len, out),
            Self::Int32(v) => widen(v.as_slice(), len, out),
            Self::Int64(v) => match v.as_slice().get(..len) {
                Some(run) => {
                    out.extend_from_slice(run);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The signed integers at the rows `at` names among the first `len`, widened to `i64`,
    /// appended to `out`.
    ///
    /// The gathered form of [`Self::signed_block`], for the rows a filter kept. Widening the whole
    /// run and then picking the kept rows out of it is a pass over every row and a second over the
    /// kept ones, where this is the one pass. `false`, leaving `out` as it found it, where
    /// [`Self::signed_block`] says `false`, and for a row that is not among the first `len`.
    #[must_use]
    pub fn signed_gather(&self, len: usize, at: &[u32], out: &mut Vec<i64>) -> bool {
        match self {
            Self::Int8(v) => gather_widened(v.as_slice(), len, at, out),
            Self::Int16(v) => gather_widened(v.as_slice(), len, at, out),
            Self::Int32(v) => gather_widened(v.as_slice(), len, at, out),
            Self::Int64(v) => gather_widened(v.as_slice(), len, at, out),
            _ => false,
        }
    }

    /// An unsigned integer at `index`, widened.
    #[must_use]
    pub fn unsigned_at(&self, index: usize) -> Option<u128> {
        macro_rules! widened {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match self {
                    $(Self::$variant(v) => v.get(index).map(|&x| u128::from(x)),)+
                    _ => None,
                }
            };
        }
        crate::for_each_layout!(unsigned, widened)
    }

    /// The string at `index`, for a `Varlen`.
    #[must_use]
    pub fn str_at(&self, index: usize) -> Option<&str> {
        match self {
            Self::Varlen(column) => column.get(index),
            _ => None,
        }
    }

    /// The bytes at `index`, for a `Varlen`, whatever they are.
    ///
    /// What a `BLOB` reads through, since the bytes of one are not required to be text and
    /// [`Self::str_at`] answers `None` for the ones that are not.
    #[must_use]
    pub fn bytes_at(&self, index: usize) -> Option<&[u8]> {
        match self {
            Self::Varlen(column) => column.bytes(index),
            _ => None,
        }
    }
}

/// A type, a length, a validity representation and some data.
#[derive(Debug, Clone, PartialEq)]
pub struct Vector {
    ty: LogicalType,
    len: usize,
    validity: Validity,
    body: Body,
}

/// What the vector holds, which is what its form is decided by.
#[derive(Debug, Clone, PartialEq)]
enum Body {
    Flat(Data),
    Constant(Box<Value>),
    Sequence {
        start: i64,
        step: i64,
    },
    /// The values are behind an `Arc` rather than a `Box` because slicing shares them.
    ///
    /// A dictionary vector is cut once per chunk and the dictionary itself is the same dictionary
    /// every time, so a `Box` meant a copy of every value in it per cut. On the ClickBench columns
    /// that are dictionary encoded the dictionary is larger than the chunk of codes pointing into
    /// it, and copying it was ten percent of the cycles of reading the file.
    ///
    /// Nothing here mutates a dictionary in place, so sharing one is only ever a read, and the one
    /// place that wants an owned copy of the values is [`compose`], which asks for one.
    Dictionary {
        codes: Buffer<u32>,
        values: Arc<Vector>,
        stable: bool,
    },
    /// Integer codes of `width` bits each, packed end to end, each one an offset from `base`.
    ///
    /// Row `r` is the `width` bits starting at bit `(offset + r) * width`, read little end first, so
    /// a code that straddles a word boundary has its low bits in the earlier word. `offset` is what
    /// lets a cut of a packed column be free: the bits are not byte aligned, so a slice either
    /// repacks or remembers where it starts, and remembering is one addition per read.
    ///
    /// The words are behind an `Arc` for the reason the dictionary's values are. A page is packed
    /// once and cut into chunk sized pieces, and copying the words per cut would undo most of what
    /// the packing saved.
    Packed {
        words: Arc<Vec<u64>>,
        width: u32,
        base: i128,
        offset: usize,
    },
    /// The views of a string column, over an arena that other vectors are reading at the same time.
    ///
    /// The views are owned because a cut is a different run of views, and the arena is shared
    /// because a cut is the same bytes. That split is the whole form: sixteen bytes a row move and
    /// the payload does not, however many cuts a page is taken in.
    ///
    /// A row's bytes are found the same way [`StringColumn`] finds them, through
    /// [`StringView::bytes_in`], so a short string never reads the arena at all and the two ways of
    /// holding strings cannot answer a row differently.
    Views {
        views: Vec<StringView>,
        arena: Arc<Buffer<u8>>,
    },
    /// Text owned by a storage source and fetched by position.
    ExternalText {
        source: Arc<dyn TextSource>,
    },
    /// The FSST codes of every row, end to end, with one symbol table over all of them.
    ///
    /// A span rather than a run of offsets, because a gather keeps this form and a gather puts the
    /// rows in an order the codes are not in. Eight bytes a row either way, and the span is the one
    /// that survives being permuted.
    ///
    /// The codes and the table are shared for the reason a dictionary's values are: one table is
    /// trained per page and every chunk cut out of it points at the same one. A table is sixty five
    /// thousand hash slots, so a table per chunk would cost more than the compression saves.
    Coded {
        codes: Arc<Vec<u8>>,
        spans: Vec<(u32, u32)>,
        table: Arc<SymbolTable>,
    },
    /// One value per run, with the row each run ends at, exclusive and increasing.
    ///
    /// Ends rather than lengths, because every reader of this wants to know which run holds a row
    /// and ends answer that with a binary search while lengths answer it with a running total. The
    /// two are the same information and only one of them is the one that gets asked for.
    ///
    /// The values are behind an `Arc` for the reason the dictionary's are: a page is cut into chunk
    /// sized pieces and the values are the same values every time.
    Runs {
        ends: Vec<u32>,
        values: Arc<Vector>,
    },
    /// One child vector holding every element of every row, and a start and a length per row.
    ///
    /// Start and length rather than the run of offsets Arrow carries, because offsets say where a
    /// row ends by saying where the next one begins, and that is only true while the rows are in
    /// order and none is skipped. A gather permutes the rows and a filter drops them, both of which
    /// this form has to survive without copying the child, so each row says where its own elements
    /// are and nothing is implied about its neighbour.
    ///
    /// The child is behind an `Arc` for the reason a dictionary's values are. A cut of a list column
    /// is the entries and nothing else, so a page of lists taken in chunk sized pieces holds one
    /// child however many pieces it is read in, and the elements outside the cut stay reachable but
    /// unreferenced rather than being copied out.
    ///
    /// A null list and an empty list are different rows and this is where the difference lives. A
    /// null is the validity mask at this level being false, the same as for any other type, and its
    /// entry is `(start, 0)` and never read. An empty list is a valid row whose entry is `(start, 0)`
    /// as well. So the entry alone does not say which one a row is, the mask does, which is the same
    /// division of labour every other form here uses.
    ///
    /// A `MAP` is stored here too, with a [`Body::Fields`] child of `key` and `value`. Everything above
    /// is true of it unchanged, which is the point of storing it this way: the cut, the gather and the
    /// null rule are written once and a map inherits all three.
    Nested {
        entries: Vec<(u32, u32)>,
        child: Arc<Vector>,
    },
    /// One child vector per field, in the order the type names them, each as long as this vector.
    ///
    /// No entries, which is the whole difference from [`Body::Nested`]. A list row is a run of
    /// elements so it needs to say where its run is, and a struct row is one value per field so row
    /// `r` of field `f` is position `r` of child `f` and there is nothing to record. That makes a cut
    /// a cut of every child and a gather a gather of every child, both at the same positions, rather
    /// than a rewrite of an index.
    ///
    /// The children are behind an `Arc` for the reason a dictionary's values are, and it pays off less
    /// often here. A cut of a list column shares its child untouched because the entries carry the
    /// range, and a cut of a struct column has to cut each child, so the sharing only survives the
    /// cases where nothing moves. It is still worth having, because a struct of a hundred fields
    /// handed between operators is a hundred pointers rather than a hundred columns.
    ///
    /// A null struct is the validity mask at this level being false and says nothing about the
    /// children, which still hold whatever was put in them at that row. That is DuckDB's behaviour and
    /// it is the reason this form cannot decide a row is null by looking down: the mask is the answer,
    /// the same as it is for a list.
    Fields {
        children: Vec<Arc<Vector>>,
    },
    /// Row `r` is row `rids[offset + r]` of `source`, and is null where that is [`NO_ROW`].
    ///
    /// Late materialization written into the type system. A link join emits one of these per
    /// projected parent column and reads nothing out of the parent at all, so a column that is
    /// projected but never inspected is read once at the end for the rows that reached the end, and
    /// a column used in a filter is filtered in this form over the distinct parent rows that were
    /// actually reached rather than once per child row.
    ///
    /// The `rids` are shared and carry an `offset` for the reason [`Body::Packed`] carries one: a
    /// link join fills one buffer of parent rows per child chunk and then the pipeline cuts it, and
    /// a cut that copied the ids would spend more moving them than the gather it is describing
    /// costs. Sharing makes a cut two words.
    ///
    /// [`NO_ROW`] is the whole of the outer join story here. Section 5.2 says a left link join keeps
    /// the child rows whose link is the no parent sentinel and gathers null for them, and an inner
    /// one drops them, so the operator decides which rows exist and this decides only what they
    /// hold. That keeps the validity of a gather derivable rather than stored: a row is null when
    /// its id is [`NO_ROW`] or when the source row it names is null, which is two loads and no
    /// allocation, and the bitmap is materialized only when a kernel asks for one.
    Gathered {
        source: Arc<Vector>,
        rids: Arc<Vec<u32>>,
        offset: usize,
    },
}

/// Random access to immutable text kept by a storage reader.
pub trait TextSource: std::fmt::Debug + Send + Sync {
    /// Number of values available.
    fn len(&self) -> usize;
    /// Whether this source has no values.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Bytes at one position, or no value when the position is outside the source.
    fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>>;
    /// Byte length at one position without requiring the payload when the source has an index.
    fn bytes_len_at(&self, index: usize) -> Result<Option<usize>> {
        Ok(self.bytes_at(index)?.map(<[u8]>::len))
    }
    /// The byte length at each of `indices`, appended to `into` in the same order, and zero for a
    /// position the source does not have.
    ///
    /// The same answers as [`bytes_len_at`](Self::bytes_len_at) a position at a time, which is what
    /// the default does. A source overrides it when it can answer a run of positions for less than
    /// the run of calls: a length asked once per row goes through a dispatch here, a dispatch in the
    /// vector and a `Result` at each, and on a column whose lengths are one load each that was most
    /// of what `STRLEN` cost. Appended rather than written into place, so that the caller has no
    /// zeroed buffer to make first only for every slot of it to be written over.
    fn bytes_lens_at(&self, indices: &[u32], into: &mut Vec<i64>) -> Result<()> {
        into.reserve(indices.len());
        for &index in indices {
            let len = self.bytes_len_at(index as usize)?.unwrap_or_default();
            into.push(i64::try_from(len).unwrap_or(i64::MAX));
        }
        Ok(())
    }
    /// The length in characters at each of `indices`, appended to `into` in the same order, and
    /// zero for a position the source does not have.
    ///
    /// What `length` asks for, where [`bytes_lens_at`](Self::bytes_lens_at) is what `strlen` asks
    /// for. Counting characters means looking at the bytes, and the default does that through
    /// [`bytes_at`](Self::bytes_at), which is right for a source that keeps its values anyway. A
    /// source that decodes a block to answer `bytes_at` keeps that block for as long as it lives,
    /// so a scan of `length` over a whole column ends up holding the whole column decoded. Such a
    /// source overrides this and keeps the counts instead of the bytes.
    fn chars_lens_at(&self, indices: &[u32], into: &mut Vec<i64>) -> Result<()> {
        into.reserve(indices.len());
        for &index in indices {
            let bytes = self.bytes_at(index as usize)?.unwrap_or_default();
            // A continuation byte of UTF-8 is `0b10xx_xxxx`, and every other byte starts a
            // character, so counting the bytes that are not continuations counts the characters.
            let characters = bytes.iter().filter(|byte| (**byte as i8) >= -0x40).count();
            into.push(i64::try_from(characters).unwrap_or(i64::MAX));
        }
        Ok(())
    }
    /// Hands `body` the values from `first` up to at most `limit`, and answers where it stopped.
    ///
    /// The point of it is what it does not do, which is keep what it read.
    /// [`bytes_at`](Self::bytes_at) hands back a borrow, so a source that decodes a block to answer
    /// it has to hold that block for as long as the source lives, and a reader that walks the whole
    /// source therefore ends up holding the whole thing decoded. On the ClickBench `URL` dictionary
    /// that is 4.2 GB resident to answer one `LIKE`, and none of it is read twice.
    ///
    /// A caller that means to walk a stretch of values once calls this instead and gets the bytes
    /// on loan for the length of the call. The source decides how much it hands over at a time,
    /// which for a blocked payload is the rest of the block it had to decode anyway, and answers
    /// with one past the last value it visited so the caller can come back for the next stretch.
    /// The answer is always above `first` where `first` is a value this source has, so a loop on it
    /// finishes.
    ///
    /// The default hands over one value through `bytes_at` and is correct for every source. It is
    /// also pointless for a source that keeps everything anyway, which is every source built in
    /// memory, and that is the right default for exactly that reason.
    fn sweep(
        &self,
        first: usize,
        limit: usize,
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        if first >= limit.min(self.len()) {
            return Ok(first);
        }
        body(first, self.bytes_at(first)?.unwrap_or_default())?;
        Ok(first + 1)
    }
    /// Hands `body` the value at each of `indices`, in whatever order suits the source, with the
    /// position in `indices` it belongs to.
    ///
    /// The whole vector twin of [`bytes_at`](Self::bytes_at), for a kernel that reads every row of
    /// a vector once and writes something per row, which is what `lower`, `upper` and `substring`
    /// do. Read a row at a time, a source that decodes a block to answer `bytes_at` has to keep
    /// every block a row lands in for as long as the source lives, because the borrow it hands back
    /// says so. Handed a whole vector of positions at once it can put them in block order, decode
    /// each block once for the call and decide for itself whether that block is worth keeping.
    ///
    /// A position the source does not have gets the empty value, which is what a row at a time
    /// read turns its missing value into. The default reads through `bytes_at` in the order given,
    /// which is right for every source that keeps its values anyway.
    fn visit_at(
        &self,
        indices: &[u32],
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for (at, &index) in indices.iter().enumerate() {
            body(at, self.bytes_at(index as usize)?.unwrap_or_default())?;
        }
        Ok(())
    }
    /// Whether the payload block holding `first` might contain `literal` in any value.
    ///
    /// A false answer is a proof that every value in the block misses. A source without a stored
    /// substring signature answers true, which keeps the ordinary exact comparison authoritative.
    fn might_contain(&self, first: usize, literal: &[u8]) -> Result<bool> {
        let _ = (first, literal);
        Ok(true)
    }
    /// Hands over the values at `indices`, which rise, without keeping what reading them decoded.
    ///
    /// The scattered twin of [`sweep`](Self::sweep). A caller that wants a few hundred values spread
    /// over the whole source once, which is what turning a frequency synopsis's codes into values
    /// is, would otherwise leave every block it touched decoded and held for the rest of the
    /// source's life. On ClickBench `SearchPhrase` that is a hundred and twenty five blocks, the
    /// larger part of what a query answered out of the synopsis was holding.
    ///
    /// `body` is told the position in `indices` and the bytes. The default reads through
    /// `bytes_at`, which is right for every source that keeps everything anyway.
    fn visit(
        &self,
        indices: &[usize],
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for (at, &index) in indices.iter().enumerate() {
            body(at, self.bytes_at(index)?.unwrap_or_default())?;
        }
        Ok(())
    }
    /// Resident bytes retained by this source.
    fn footprint(&self) -> usize;
    /// How many ranks this source's sorted value order has, when it has one.
    ///
    /// A rank is a position in the values sorted by their bytes, so rank zero is the smallest value
    /// and rank `ranks() - 1` is the largest. A storage format that keeps a dictionary for a whole
    /// column can afford to sort the distinct values once when it writes the file, and what that
    /// buys is a binary search where a reader that only knows the values are distinct has to ask
    /// every one of them whether it matches.
    ///
    /// `None` means the source does not know its order, which is the honest answer for anything
    /// built in memory and for a file written before its format stored one. Nothing is allowed to
    /// depend on this for correctness, only for speed.
    ///
    /// A source that answers with `Some` promises the ranks cover every value it has, and that
    /// [`compare_rank`](Self::compare_rank) is consistent with an ordering in which the values are
    /// strictly increasing. Strictly, which is to say the values are distinct, because what reads
    /// this searches it, and a search of a run of equal values finds one of them rather than all of
    /// them. A source that holds the same value twice must answer `None` here even though it could
    /// sort itself perfectly well.
    fn ranks(&self) -> Option<usize> {
        None
    }
    /// How the value at `rank` compares against `wanted`.
    ///
    /// This is a method rather than a slice of positions the caller indexes because the answer is
    /// the only thing a search wants, and a source that knows that can answer most probes without
    /// reading a value at all. A file that stores the first few bytes of each value in rank order
    /// settles every probe from those bytes except the ones where two values start the same way,
    /// and the payload stays untouched. A caller handed positions instead would have to read a
    /// value per probe, which for a dictionary of half a million entries spread over thirty
    /// megabytes is a fresh block of the file every time.
    ///
    /// Only called for a rank below [`ranks`](Self::ranks), so the default is the error a source
    /// that has no order should never be asked to produce.
    fn compare_rank(&self, rank: usize, wanted: &[u8]) -> Result<Ordering> {
        let _ = (rank, wanted);
        Err(Error::internal("a text source without a sorted order was asked to compare a rank"))
    }
    /// How many values sort before `wanted`, and whether one of them is `wanted`.
    ///
    /// The whole search rather than a probe of it, so that a source which can answer the same
    /// question twice without repeating the work is allowed to. The default runs the search through
    /// [`compare_rank`](Self::compare_rank) and remembers nothing, which is right for a source whose
    /// probes are cheap.
    ///
    /// The reason it is on the trait at all is the top N. `ORDER BY <varchar> LIMIT 10` asks once a
    /// chunk whether anything left can beat the worst candidate, and the worst candidate stops
    /// changing long before the chunks run out, so nearly every one of those searches is the one
    /// before it asked again. A probe of a file backed dictionary is not cheap: it settles on the
    /// stored head where it can and reads a value where it cannot, and reading a value means
    /// decoding the payload block it sits in. On ClickBench 25 that search was 29 percent of the
    /// query's instructions and the block decoding under it another 40.
    ///
    /// Only called when [`ranks`](Self::ranks) is `Some`, and `ranks` is what it answered.
    fn below(&self, ranks: usize, wanted: &[u8]) -> Result<(usize, bool)> {
        search_below(self, ranks, wanted)
    }
    /// The position of the value at `rank`, which is what a search returns once it has found one.
    ///
    /// Called about once per search rather than once per probe, so unlike
    /// [`compare_rank`](Self::compare_rank) it is free to be the expensive one.
    fn code_at_rank(&self, rank: usize) -> Result<u32> {
        let _ = rank;
        Err(Error::internal("a text source without a sorted order was asked for a rank"))
    }
    /// The rank of every value, in position order, when the source can hand the whole map over.
    ///
    /// This is [`code_at_rank`](Self::code_at_rank) turned round, and it is a separate method
    /// because the two are wanted by opposite kinds of reader. A search wants one code out of a
    /// rank and probes a handful of times, so it reads the order a block at a time and leaves the
    /// rest alone. A min or a max over a grouped column wants a rank out of a code once per row,
    /// and a walk of the order per row costs far more than reading the order once and turning it
    /// round. What that buys is a comparison of two integers where the alternative is a fetch of
    /// two strings out of a payload the size of the column.
    ///
    /// The slice is indexed by position and is as long as [`len`](Self::len), so a caller holding a
    /// dictionary code indexes it directly.
    ///
    /// `None` from a source with no order, and from one with an order it would rather not invert.
    /// Nothing depends on this for correctness, only for speed.
    fn code_ranks(&self) -> Option<&[u32]> {
        None
    }
    /// Whether another source presents the same values.
    fn equal(&self, other: &dyn TextSource) -> bool {
        self.len() == other.len()
            && (0..self.len()).all(|index| {
                matches!(
                    (self.bytes_at(index), other.bytes_at(index)),
                    (Ok(left), Ok(right)) if left == right
                )
            })
    }
}

impl PartialEq for dyn TextSource {
    fn eq(&self, other: &Self) -> bool {
        self.equal(other)
    }
}

/// The binary search behind [`TextSource::below`], written once so an override can still use it.
///
/// A source that remembers its answers overrides `below` to look in what it remembers first, and
/// then it still has to do the search when it does not find one. This is that search. It carries on
/// past an equal probe to the first rank holding the value, so what it returns is a boundary rather
/// than wherever the halving happened to touch down, and the values are distinct so there is exactly
/// one such rank.
///
/// # Errors
///
/// Whatever [`TextSource::compare_rank`] gives for a probe.
pub fn search_below<S>(source: &S, ranks: usize, wanted: &[u8]) -> Result<(usize, bool)>
where
    S: TextSource + ?Sized,
{
    let mut low = 0;
    let mut high = ranks;
    let mut equal = false;
    while low < high {
        let middle = low + (high - low) / 2;
        match source.compare_rank(middle, wanted)? {
            Ordering::Less => low = middle + 1,
            Ordering::Greater => high = middle,
            Ordering::Equal => {
                equal = true;
                high = middle;
            }
        }
    }
    Ok((low, equal))
}

impl Vector {
    /// A flat vector of `data`, all valid.
    ///
    /// # Errors
    ///
    /// If the data's physical layout is not the one the type calls for. That check is here rather
    /// than left to the caller because a vector whose type and layout disagree is a wrong answer
    /// waiting to be read out, and it costs one comparison at construction to prevent.
    pub fn flat(ty: LogicalType, data: Data) -> Result<Self> {
        let len = data.len();
        if !matches!(data, Data::Empty) && layout_of(&data) != ty.physical() {
            return Err(Error::internal(format!(
                "a {ty} vector cannot hold {:?} data",
                layout_of(&data)
            )));
        }
        Ok(Self { ty, len, validity: Validity::AllValid, body: Body::Flat(data) })
    }

    /// A flat vector built from single values, with the nulls among them turning into validity.
    ///
    /// The slow way in, and the only way in that anything outside this crate has. It is what an
    /// `INSERT`, a `VALUES` clause and a test build a column with, all of which arrive holding
    /// values rather than a run of `i32`. Nothing on a scan path calls it: a scan produces a run of
    /// data directly and hands it to [`Self::flat`].
    ///
    /// # Errors
    ///
    /// If a value is not one the type can hold, or if the type is one there is no vector for yet,
    /// which today means `ARRAY` and `UNION`. A `LIST`, a `STRUCT` and a `MAP` are routed to their own
    /// builders and come back built.
    pub fn from_values(ty: LogicalType, values: &[Value]) -> Result<Self> {
        match &ty {
            LogicalType::List(element) => {
                return Self::list_from_values(element.as_ref().clone(), values);
            }
            LogicalType::Struct(fields) => return Self::struct_from_values(fields, values),
            LogicalType::Map(key, value) => {
                return Self::map_from_values(key.as_ref().clone(), value.as_ref().clone(), values);
            }
            _ => {}
        }
        let mut data = empty_data_for(&ty)?;
        for value in values {
            push_value(&mut data, value)?;
        }
        let validity = Validity::from_iter(values.len(), |index| !values[index].is_null());
        Ok(Self { ty, len: values.len(), validity, body: Body::Flat(data) })
    }

    /// A list vector of `element`, built from one [`Value::List`] per row.
    ///
    /// The elements of every row go into one child vector end to end, so a row's elements are a
    /// contiguous range of it and a row is a start and a length into it. That is what makes a cut of
    /// this form the entries and nothing else.
    ///
    /// A null row contributes no elements and gets an entry of length zero, which is the same entry
    /// an empty list gets. The two are told apart by the validity mask rather than by the entry, for
    /// the reason written on [`Body::Nested`].
    fn list_from_values(element: LogicalType, values: &[Value]) -> Result<Self> {
        let mut flat = Vec::new();
        let mut entries = Vec::with_capacity(values.len());
        for value in values {
            let start = u32::try_from(flat.len())
                .map_err(|_| Error::internal("a list column with more than u32 elements in it"))?;
            match value {
                Value::Null => entries.push((start, 0)),
                Value::List { values: held, .. } => {
                    let len = u32::try_from(held.len())
                        .map_err(|_| Error::internal("a list longer than u32"))?;
                    flat.extend_from_slice(held);
                    entries.push((start, len));
                }
                other => {
                    return Err(Error::internal(format!(
                        "{other:?} does not belong in a list vector"
                    )));
                }
            }
        }
        // The element type is the column's rather than any one value's. A `Value::List` carries what
        // it thinks it is empty of, and a column built from a row of `INTEGER[]` and a row of
        // `[]::NULL[]` would otherwise take its type from whichever row came first.
        let child = Self::from_values(element, &flat)?;
        let validity = Validity::from_iter(values.len(), |index| !values[index].is_null());
        Ok(Self {
            ty: LogicalType::list(child.ty.clone()),
            len: values.len(),
            validity,
            body: Body::Nested { entries, child: Arc::new(child) },
        })
    }

    /// A list vector over a child that already exists, one entry per row.
    ///
    /// What a scan and a list returning kernel build, both of which produce the elements in bulk and
    /// then say which row each range belongs to. Every row is valid, since a caller with nulls to
    /// record adds them with [`Self::with_validity`].
    ///
    /// # Errors
    ///
    /// If an entry runs past the end of the child, which would be a row that reads elements belonging
    /// to nobody and is the one mistake this form makes easy.
    pub fn list(entries: Vec<(u32, u32)>, child: Vector) -> Result<Self> {
        let reach = child.len();
        for &(start, len) in &entries {
            if start as usize + len as usize > reach {
                return Err(Error::internal(format!(
                    "a list entry of {len} at {start} in a child of {reach}"
                )));
            }
        }
        Ok(Self {
            ty: LogicalType::list(child.ty.clone()),
            len: entries.len(),
            validity: Validity::AllValid,
            body: Body::Nested { entries, child: Arc::new(child) },
        })
    }

    /// A struct vector of `fields`, built from one [`Value::Struct`] per row.
    ///
    /// One pass per field rather than one pass per row, because each field becomes its own child
    /// vector and a child is built from a run of values of one type. So a struct of three fields over
    /// a thousand rows is three calls to [`Self::from_values`] and not a thousand.
    ///
    /// The fields are matched by name and not by position. A `Value::Struct` carries its names, and a
    /// caller that built one in a different order from the type's would otherwise get the values
    /// silently transposed into the wrong columns, which is the kind of wrong answer that reads as
    /// right. A row missing a field the type names is an error rather than a null for the same reason.
    ///
    /// A null row is a null in every child as well as a false bit in the mask here. [`Body::Fields`]
    /// says a null struct is allowed to have readable children and that is about a struct built out of
    /// children that already exist, where whatever is underneath is the caller's. Built from values
    /// there is nothing underneath to keep, so the children get the null.
    fn struct_from_values(fields: &[Field], values: &[Value]) -> Result<Self> {
        let mut children = Vec::with_capacity(fields.len());
        // An unnamed struct has no names to match on, so its fields are taken by place.
        let unnamed = Field::unnamed(fields);
        for (at, field) in fields.iter().enumerate() {
            let mut column = Vec::with_capacity(values.len());
            for value in values {
                column.push(match value {
                    Value::Null => Value::Null,
                    Value::Struct(held) if unnamed => held
                        .get(at)
                        .map(|(_, held)| held.clone())
                        .ok_or_else(|| Error::internal("a tuple row shorter than its type"))?,
                    Value::Struct(held) => held
                        .iter()
                        .find(|(name, _)| *name == field.name)
                        .map(|(_, held)| held.clone())
                        .ok_or_else(|| {
                            Error::internal(format!(
                                "a struct row with no {} field in it",
                                field.name
                            ))
                        })?,
                    other => {
                        return Err(Error::internal(format!(
                            "{other:?} does not belong in a struct vector"
                        )));
                    }
                });
            }
            children.push(Arc::new(Self::from_values(field.ty.clone(), &column)?));
        }
        let validity = Validity::from_iter(values.len(), |index| !values[index].is_null());
        Ok(Self {
            ty: LogicalType::Struct(fields.to_vec()),
            len: values.len(),
            validity,
            body: Body::Fields { children },
        })
    }

    /// A struct vector over children that already exist, one per field.
    ///
    /// What a scan and a struct returning kernel build, both of which produce each field as a column
    /// and then put them side by side. Every row is valid, since a caller with nulls to record adds
    /// them with [`Self::with_validity`].
    ///
    /// # Errors
    ///
    /// If there are no fields, or if the children are not all the same length. The first is not a
    /// fussy restriction: a struct vector with no children has no child to take its length from, so a
    /// zero field struct column would be a length with nothing to check it against, and a caller that
    /// wants a column of empty structs wants a constant vector of one.
    pub fn structure(children: Vec<(String, Vector)>) -> Result<Self> {
        let Some((_, first)) = children.first() else {
            return Err(Error::internal("a struct vector of no fields, which has no length"));
        };
        let len = first.len();
        for (name, child) in &children {
            if child.len() != len {
                return Err(Error::internal(format!(
                    "a {} field of {} rows beside a struct of {len}",
                    name,
                    child.len()
                )));
            }
        }
        let fields = children
            .iter()
            .map(|(name, child)| Field::new(name.clone(), child.ty.clone()))
            .collect();
        let children = children.into_iter().map(|(_, child)| Arc::new(child)).collect();
        Ok(Self {
            ty: LogicalType::Struct(fields),
            len,
            validity: Validity::AllValid,
            body: Body::Fields { children },
        })
    }

    /// The children, for a struct vector, and `None` for any other form.
    ///
    /// The accessor a kernel over a struct column reads, and the reason field extraction is free:
    /// picking one field out of a struct is picking one of these, so a projection of `s.a` hands back
    /// a vector that already exists rather than reading a row at a time and rebuilding a column.
    #[must_use]
    pub fn struct_parts(&self) -> Option<&[Arc<Self>]> {
        match &self.body {
            Body::Fields { children } => Some(children),
            _ => None,
        }
    }

    /// A map vector, built from one [`Value::Map`] per row.
    ///
    /// A map is a list whose child is a two field struct of keys and values, which is what DuckDB
    /// stores and what Arrow and Parquet store, so this is the list builder and the struct builder
    /// composed rather than a third layout. The keys of every row go into one column end to end, the
    /// values into another beside it, and a row is a start and a length into the pair.
    ///
    /// The field names are [`MAP_KEY`] and [`MAP_VALUE`] because those are the names DuckDB gives them
    /// and the names anything reading a Parquet map field will expect to find.
    ///
    /// A null row and an empty map are both an entry of length zero, told apart by the validity mask,
    /// for the reason written on [`Body::Nested`].
    fn map_from_values(key: LogicalType, value: LogicalType, values: &[Value]) -> Result<Self> {
        let mut keys = Vec::new();
        let mut held = Vec::new();
        let mut entries = Vec::with_capacity(values.len());
        for row in values {
            let start = u32::try_from(keys.len())
                .map_err(|_| Error::internal("a map column with more than u32 entries in it"))?;
            match row {
                Value::Null => entries.push((start, 0)),
                Value::Map { entries: pairs, .. } => {
                    let len = u32::try_from(pairs.len())
                        .map_err(|_| Error::internal("a map with more than u32 entries"))?;
                    for (one, other) in pairs {
                        keys.push(one.clone());
                        held.push(other.clone());
                    }
                    entries.push((start, len));
                }
                other => {
                    return Err(Error::internal(format!(
                        "{other:?} does not belong in a map vector"
                    )));
                }
            }
        }
        // The two types are the column's rather than any one row's, for the reason the list builder
        // takes the element type from the column: a row that is the empty map carries whatever it was
        // built as being empty of, and the column is not entitled to take its type from that.
        let child = Self::structure(vec![
            (MAP_KEY.to_string(), Self::from_values(key, &keys)?),
            (MAP_VALUE.to_string(), Self::from_values(value, &held)?),
        ])?;
        let ty = LogicalType::map(
            fields_of(&child.ty)[0].ty.clone(),
            fields_of(&child.ty)[1].ty.clone(),
        );
        let validity = Validity::from_iter(values.len(), |index| !values[index].is_null());
        Ok(Self {
            ty,
            len: values.len(),
            validity,
            body: Body::Nested { entries, child: Arc::new(child) },
        })
    }

    /// A map vector over a pair of columns that already exist, one entry per row.
    ///
    /// What a scan and a map returning kernel build. The keys and the values are two columns of the
    /// same length, and each row of the map is the same range of both. Every row is valid, since a
    /// caller with nulls to record adds them with [`Self::with_validity`].
    ///
    /// # Errors
    ///
    /// If the two columns are different lengths, or if an entry runs past the end of them.
    pub fn map(entries: Vec<(u32, u32)>, keys: Vector, values: Vector) -> Result<Self> {
        let key = keys.ty.clone();
        let value = values.ty.clone();
        let child =
            Self::structure(vec![(MAP_KEY.to_string(), keys), (MAP_VALUE.to_string(), values)])?;
        let mut vector = Self::list(entries, child)?;
        vector.ty = LogicalType::map(key, value);
        Ok(vector)
    }

    /// The entries and the two columns, for a map vector, and `None` for anything else.
    ///
    /// Reaches through the struct child that a map is stored as, so that a kernel over a map column
    /// reads the keys and the values as the two columns they are rather than having to know that the
    /// pair is spelled as a struct underneath.
    #[must_use]
    pub fn map_parts(&self) -> Option<MapParts<'_>> {
        if !matches!(self.ty, LogicalType::Map(_, _)) {
            return None;
        }
        let (entries, child) = self.list_parts()?;
        let [keys, values] = child.struct_parts()? else { return None };
        Some((entries, keys, values))
    }

    /// The entries and the child, for a list vector, and `None` for any other form.
    ///
    /// The accessor a kernel over a list column reads, for the reason
    /// [`Self::dictionary_parts`] exists: `unnest` over 1024 rows wants the child once and the
    /// entries once, and reading it through [`Self::value_at`] would build a `Value::List` per row
    /// and then throw every one of them away.
    ///
    /// A map answers here as well, with the struct child it is stored as, because this is a question
    /// about the layout and a map's layout is a list's. A caller that wants the keys and the values as
    /// two columns wants [`Self::map_parts`], which reaches through that child.
    #[must_use]
    pub fn list_parts(&self) -> Option<(&[(u32, u32)], &Self)> {
        match &self.body {
            Body::Nested { entries, child } => Some((entries, child)),
            _ => None,
        }
    }

    /// A vector of `len` copies of one value.
    ///
    /// Costs one value regardless of the length, which is what makes a literal in a predicate free
    /// and what makes a projection of a constant free.
    #[must_use]
    pub fn constant(ty: LogicalType, value: Value, len: usize) -> Self {
        let validity = if value.is_null() { Validity::AllInvalid } else { Validity::AllValid };
        Self { ty, len, validity, body: Body::Constant(Box::new(value)) }
    }

    /// A vector of `len` values starting at `start` and stepping by `step`.
    ///
    /// This is what a row identifier column is, and it costs sixteen bytes rather than eight
    /// kilobytes. A scan that produces row ids for a later fetch produces one of these.
    #[must_use]
    pub fn sequence(start: i64, step: i64, len: usize) -> Self {
        Self {
            ty: LogicalType::BigInt,
            len,
            validity: Validity::AllValid,
            body: Body::Sequence { start, step },
        }
    }

    /// A vector of codes into a smaller vector of distinct values.
    ///
    /// The form the whole M3 thesis rests on. A dictionary vector handed to a group by is an
    /// integer column, and an aggregate over one is an aggregate over integers no matter what the
    /// logical type says.
    ///
    /// A dictionary over a dictionary is composed into one level here rather than left as two, so
    /// the form has a depth of one always and a kernel that reads [`Self::dictionary_parts`] is
    /// reading the values rather than another layer of codes. Two filters over the same chunk build
    /// the second case and four conjuncts pushed down separately build four of it.
    ///
    /// The cost of leaving them stacked turned out to be a cliff rather than a slope. Every loop in
    /// `rudb-kernels` reaches for the values behind the codes with [`Self::data`], a dictionary
    /// pointing at a dictionary has no data to hand back, so the second level does not make the
    /// kernels slower, it turns them off and drops the work onto the row at a time path that exists
    /// to be correct rather than fast. Measured on server3 over a chunk of two numeric columns and a
    /// consumer of two vectorized passes, one level reads at 3.5 nanoseconds a row and two levels at
    /// 104, and the third and fourth levels cost almost nothing more because the first one had
    /// already given up everything there was to give. Composing is one pass over the outer codes,
    /// which the range check above is already making.
    ///
    /// The one dictionary that is not composed past is one carrying a validity of its own. A
    /// dictionary is built all valid and only [`Self::with_validity`] can change that, so such a
    /// vector is saying that its nulls are at this level rather than in the values it points at, and
    /// composing past it would drop them.
    ///
    /// # Errors
    ///
    /// If any code is past the end of the value vector.
    pub fn dictionary(codes: Vec<u32>, values: Vector) -> Result<Self> {
        Self::dictionary_over(codes, Arc::new(values))
    }

    /// The same, over a set of values somebody else is holding too.
    ///
    /// The body holds its values in an `Arc` either way, so a caller that already has one has
    /// nothing to hand over but a pointer. The caller this is for is a Parquet chunk: one dictionary
    /// page serves every data page of the chunk, and going through [`Self::dictionary`] meant
    /// copying the whole dictionary into each page's vector on the way to putting it in an `Arc`
    /// that then had a single holder. On a ClickBench scan that copy was sixteen percent of the
    /// instructions the query ran.
    ///
    /// Composing a dictionary over a dictionary keeps the handle too. The leaf of the stack is what
    /// the composed dictionary points at and neither its values nor anything about it changes, so
    /// there is nothing to own and the new dictionary shares the same leaf the old one did.
    ///
    /// The range check takes the highest code rather than stopping at the first bad one. Stopping
    /// early sounds cheaper and is not, because a loop that can exit anywhere cannot be vectorized
    /// and a running maximum can, and the only run that would have exited early is the one about to
    /// fail the query anyway. Every other run reads the whole of `codes` either way. It was 5.2
    /// percent of a ClickBench scan as a `find`.
    ///
    /// # Errors
    ///
    /// If any code is past the end of the value vector.
    pub fn dictionary_over(codes: Vec<u32>, values: Arc<Vector>) -> Result<Self> {
        if !below(&codes, values.len()) {
            let highest = codes.iter().copied().fold(0, u32::max);
            return Err(Error::internal(format!(
                "dictionary code {highest} is past the end of a {} value dictionary",
                values.len()
            )));
        }
        let (codes, values) = compose(codes, values);
        Ok(Self {
            ty: values.ty.clone(),
            len: codes.len(),
            validity: Validity::AllValid,
            body: Body::Dictionary { codes: Buffer::from_vec(codes), values, stable: false },
        })
    }

    /// A dictionary whose codes keep the same meaning across every page of its source.
    pub fn stable_dictionary(codes: Vec<u32>, values: Arc<Vector>) -> Result<Self> {
        let mut vector = Self::dictionary_over(codes, values)?;
        if let Body::Dictionary { stable, .. } = &mut vector.body {
            *stable = true;
        }
        Ok(vector)
    }

    /// A stable dictionary whose caller already found the largest code while decoding it.
    pub fn stable_dictionary_validated(
        codes: Vec<u32>,
        values: Arc<Vector>,
        highest: Option<u32>,
    ) -> Result<Self> {
        if highest.is_some_and(|code| code as usize >= values.len()) {
            return Err(Error::internal("a stable dictionary code is past its value dictionary"));
        }
        Ok(Self {
            ty: values.ty.clone(),
            len: codes.len(),
            validity: Validity::AllValid,
            body: Body::Dictionary { codes: Buffer::from_vec(codes), values, stable: true },
        })
    }

    /// One row of `source` per id, without reading any of them.
    ///
    /// What a link join emits for each of its parent columns, per `spec/graph/08-vector-engine.md`
    /// section 8.2. Row `r` is row `rids[r]` of `source`, and is null where that is [`NO_ROW`].
    ///
    /// The ids are taken by `Arc` rather than by value because one link join fills one buffer of
    /// parent rows per child chunk and then hands the same buffer to every projected parent column,
    /// so a gather of eight columns is eight pointers and one buffer. [`Self::gathered_from`] is the
    /// same thing starting part way in, which is what a cut of one produces.
    ///
    /// # Errors
    ///
    /// If an id is past the end of the source and is not [`NO_ROW`]. That check is a pass over the
    /// ids and it is the only thing standing between a link built against the wrong parent and a
    /// read of whatever happens to be at that offset, so it is not optional and it is not deferred:
    /// `spec/graph/03-the-file-format.md` section 3.1 says a stale section is ignored rather than
    /// repaired, and this is where a stale one stops being ignorable.
    pub fn gathered(source: Arc<Vector>, rids: Arc<Vec<u32>>) -> Result<Self> {
        let len = rids.len();
        Self::gathered_from(source, rids, 0, len)
    }

    /// The same, reading `len` ids starting at `offset`.
    ///
    /// # Errors
    ///
    /// If the range runs past the end of the ids, or if an id in it is past the end of the source.
    pub fn gathered_from(
        source: Arc<Vector>,
        rids: Arc<Vec<u32>>,
        offset: usize,
        len: usize,
    ) -> Result<Self> {
        let end = offset.checked_add(len).ok_or_else(|| Error::internal("a gather that wraps"))?;
        let Some(taken) = rids.get(offset..end) else {
            return Err(Error::internal(format!(
                "rows {offset} to {end} of a gather over {} ids",
                rids.len()
            )));
        };
        let rows = source.len();
        if taken.iter().any(|&rid| rid != NO_ROW && rid as usize >= rows) {
            return Err(Error::internal(format!(
                "a gathered row id is past the {rows} rows of its source"
            )));
        }
        Ok(Self {
            ty: source.ty.clone(),
            len,
            // The mask is all valid and the nulls are real, which is the same split a dictionary
            // makes: this level says every row exists and the body says what each one holds, and
            // `is_null_at` reads through to answer. A mask here would be a second copy of what the
            // ids already say and the two could disagree.
            validity: Validity::AllValid,
            body: Body::Gathered { source, rids, offset },
        })
    }

    /// The source and the ids of a gathered vector, and `None` for any other form.
    #[must_use]
    pub fn gathered_parts(&self) -> Option<(&Arc<Self>, &[u32])> {
        match &self.body {
            Body::Gathered { source, rids, offset } => {
                Some((source, rids.get(*offset..offset + self.len)?))
            }
            _ => None,
        }
    }

    /// Whether a kernel over this vector should fold over the source once and then index.
    ///
    /// Section 8.2's dispatch rule, which is one comparison and is the whole difference between a
    /// gather and a dictionary. Every kernel with a dictionary arm already folds over the values
    /// once and indexes, and that arm is right for a gather exactly when the source is shorter than
    /// the rows being answered. A dictionary always is, by construction. A gather off a parent
    /// table almost never is, and a kernel that took the dictionary arm anyway would read fifteen
    /// million parent rows to answer two thousand child ones.
    ///
    /// `false` for every other form, so a kernel can ask this without first asking what it has.
    #[must_use]
    pub fn fold_over_source(&self) -> bool {
        match &self.body {
            Body::Gathered { source, .. } => source.len() < self.len,
            _ => false,
        }
    }

    /// A vector of runs, one value each, with the row each run ends at.
    ///
    /// `ends` is exclusive and strictly increasing, so run `i` covers the rows from `ends[i - 1]` to
    /// `ends[i]` and run zero starts at nothing. The length of the vector is the last end.
    ///
    /// The depth is one, the same way a dictionary's is, and for a sharper reason. Every kernel that
    /// wants runs wants the value of a run without another search, and a run length vector over a
    /// run length vector turns one search into two and then into three. Rather than compose, this
    /// refuses: nothing in the engine builds a stacked one, because [`Self::run_encoded`] only ever
    /// reads a flat body, so a stacked one is a caller doing something by hand and the useful answer
    /// is to say so rather than to quietly do a pass of work they did not ask for.
    ///
    /// A run over a dictionary is fine and is not that case. The two forms answer different
    /// questions and a column that is both clustered and low cardinality genuinely wants both.
    ///
    /// # Errors
    ///
    /// If there is not exactly one value per run, if the ends do not increase, or if the values are
    /// themselves run length encoded.
    pub fn runs(ends: Vec<u32>, values: Vector) -> Result<Self> {
        if matches!(values.body, Body::Runs { .. }) {
            return Err(Error::internal("runs of runs, which is two searches to read one row"));
        }
        if ends.len() != values.len() {
            return Err(Error::internal(format!(
                "{} runs and {} values to put in them",
                ends.len(),
                values.len()
            )));
        }
        if ends.windows(2).any(|pair| pair[0] >= pair[1]) || ends.first() == Some(&0) {
            return Err(Error::internal("run ends that do not increase"));
        }
        let len = ends.last().copied().unwrap_or(0) as usize;
        Ok(Self {
            ty: values.ty.clone(),
            len,
            validity: Validity::AllValid,
            body: Body::Runs { ends, values: Arc::new(values) },
        })
    }

    /// The same values as runs, when there are few enough runs for that to be smaller.
    ///
    /// Costs one pass over the column to find out, which is why this is a call somebody makes rather
    /// than something a constructor does. The decision is the same arithmetic every time: a row in
    /// flat form costs one value, a run costs one value plus the four bytes of its end, so runs are
    /// smaller once there are fewer than about half as many runs as rows, and the narrower the
    /// column the more runs it takes. `RUNS_PAY_AT` is that ratio, written down rather than spelt
    /// into an `if`, because it is the number a sweep will want to move.
    ///
    /// Only a flat body is looked at. A constant and a sequence are already one value and two
    /// numbers, so there is nothing to win, and a dictionary that is also clustered is a real case
    /// that wants its codes run length encoded rather than its values, which is a different function
    /// and not this one.
    ///
    /// Two adjacent nulls are one run. Two adjacent equal values with a null between them are three,
    /// because the null is a value of the column as far as anything reading it is concerned.
    ///
    /// # Errors
    ///
    /// From the gather this does at the end, and nowhere else. A body that is not flat comes back
    /// unchanged rather than as an error, so a nested vector never reaches the part that can fail.
    pub fn run_encoded(&self) -> Result<Self> {
        let Body::Flat(data) = &self.body else {
            return Ok(self.clone());
        };
        let ends = boundaries(data, &self.validity, self.len);
        if ends.len().saturating_mul(RUNS_PAY_AT) >= self.len {
            return Ok(self.clone());
        }
        let starts: Vec<u32> =
            std::iter::once(0).chain(ends.iter().copied()).take(ends.len()).collect();
        Self::runs(ends, self.gather(&starts)?)
    }

    /// A vector of `len` integers packed `width` bits each, every one an offset from `base`.
    ///
    /// The way in for a reader that already has the packed bits, which is what a column file holds
    /// and what a network frame carries. Nothing unpacks on the way in, so a scan of a packed column
    /// hands the bits straight to the chunk and the cost of the form is paid by whoever reads a
    /// value rather than by the scan.
    ///
    /// The range check is on the two ends rather than on every code, which is the whole check. A
    /// code is between zero and `2^width - 1` by construction, so if `base` and `base + 2^width - 1`
    /// both fit the column's layout then every value does, and that is two comparisons instead of
    /// one per row.
    ///
    /// # Errors
    ///
    /// If the type is not one of the integer layouts, if the width is not between one and
    /// [`PACKED_WIDTH_MAX`], if there are not enough words for the length, or if either end of the
    /// range would not fit the type.
    pub fn packed(
        ty: LogicalType,
        words: Vec<u64>,
        width: u32,
        base: i128,
        len: usize,
    ) -> Result<Self> {
        let Some((low, high)) = layout_range(&ty) else {
            return Err(Error::internal(format!("a {ty} vector has no integer layout to pack")));
        };
        if width == 0 || width > PACKED_WIDTH_MAX {
            return Err(Error::internal(format!(
                "a packed width of {width}, which is outside 1 to {PACKED_WIDTH_MAX}"
            )));
        }
        let needed = words_for(len, width);
        if words.len() < needed {
            return Err(Error::internal(format!(
                "{} words for {len} values of {width} bits, which needs {needed}",
                words.len()
            )));
        }
        let top = base + i128::from(u64::MAX >> (64 - width));
        if base < low || top > high {
            return Err(Error::internal(format!(
                "packed values from {base} to {top}, which a {ty} cannot hold"
            )));
        }
        Ok(Self {
            ty,
            len,
            validity: Validity::AllValid,
            body: Body::Packed { words: Arc::new(words), width, base, offset: 0 },
        })
    }

    /// The same values bit packed, when the range of the column makes that smaller.
    ///
    /// Costs one pass to find the range and one to write the bits, which is why this is a call
    /// somebody makes rather than something a constructor does. It is the counterpart of
    /// [`Self::run_encoded`] and the decision has the same shape: a row flat costs the width of its
    /// layout, a row packed costs the bits the column's range needs, and the form is worth having
    /// only when the second is a good deal smaller than the first. [`PACKING_PAYS_AT`] is that
    /// ratio, written down rather than spelt into an `if`, because it is the number a sweep will
    /// want to move.
    ///
    /// Only a flat integer body is looked at. A constant and a sequence are already smaller than any
    /// packing of them, a dictionary's codes are the thing that would want packing rather than its
    /// values, and a float has no range to pack into since the bits of an `f64` are not an integer
    /// that arithmetic on the column agrees with.
    ///
    /// The range is taken over every slot including the null ones, which hold a zero. A column of
    /// large values with one null in it therefore packs a range that reaches down to zero and comes
    /// out wider than it needed to be. The alternative is a pass that consults the validity per slot
    /// to find the range and a second rule for what to write into a null slot, and this form exists
    /// to make reads cheap rather than to squeeze the last bit out of a sparse column.
    ///
    /// A column whose values are all the same packs to nothing at all, and rather than invent a zero
    /// bit code this declines and leaves it to [`Self::run_encoded`], which turns that column into
    /// one run and is smaller than any packing of it.
    ///
    /// # Errors
    ///
    /// If the packed bits and the length disagree, which would be a bug here rather than a caller
    /// doing something wrong.
    pub fn bit_packed(&self) -> Result<Self> {
        let Body::Flat(data) = &self.body else {
            return Ok(self.clone());
        };
        let Some((low, high)) = span_of(data, self.len) else {
            return Ok(self.clone());
        };
        let Some(range) = high.checked_sub(low).and_then(|range| u64::try_from(range).ok()) else {
            return Ok(self.clone());
        };
        let width = u64::BITS - range.leading_zeros();
        if width == 0 || width > PACKED_WIDTH_MAX {
            return Ok(self.clone());
        }
        // Against the bytes the rows take and not the footprint, because a window of a shared page
        // reports its share of the page. That made the answer, and so the file a load writes,
        // depend on how big the page was and how many readers it had.
        if words_for(self.len, width) * size_of::<u64>() * PACKING_PAYS_AT
            > flat_bytes(data, self.len)
        {
            return Ok(self.clone());
        }
        // A range can fit the type while that width up from the smallest value does not: a column
        // of a thousand values under `i32::MAX` needs ten bits, and ten bits up from the smallest
        // of them runs past `i32::MAX`. The packed form checks both ends of what its width can
        // say, so the base moves down until they both fit rather than the column being left flat.
        let Some(base) = packing_base(&self.ty, low, high, width) else {
            return Ok(self.clone());
        };
        let words = pack(data, self.len, base, width);
        let packed = Self::packed(self.ty.clone(), words, width, base, self.len)?;
        Ok(packed.with_validity(self.validity.clone()))
    }

    /// A vector of string views over an arena somebody else is holding too.
    ///
    /// The way in for a scan that has a page of strings and wants several chunks over it. Each chunk
    /// gets its own run of views and they all share the one arena, so the bytes are read where the
    /// page put them and nothing copies them.
    ///
    /// Every view is checked against the arena here rather than when a row is read. That is a pass
    /// over the views at construction, which is the same pass the caller just did to build them, and
    /// what it buys is that a row of this form cannot resolve to bytes that are not there. The check
    /// is on the offsets and not on the bytes, so it says nothing about whether the payload is text,
    /// which is the same promise a `BLOB` column makes.
    ///
    /// # Errors
    ///
    /// If the type is not one stored as views, or if a view points past the end of the arena.
    pub fn string_views(
        ty: LogicalType,
        views: Vec<StringView>,
        arena: Arc<Buffer<u8>>,
    ) -> Result<Self> {
        if ty.physical() != rudb_common::PhysicalType::Varlen {
            return Err(Error::internal(format!("a {ty} vector cannot hold string views")));
        }
        if views.iter().any(|view| view.bytes_in(&arena).is_none()) {
            return Err(Error::internal("a string view points past the end of its arena"));
        }
        let len = views.len();
        Ok(Self { ty, len, validity: Validity::AllValid, body: Body::Views { views, arena } })
    }

    /// A text vector whose values remain in a storage source until they are read.
    pub fn external_text(ty: LogicalType, source: Arc<dyn TextSource>) -> Result<Self> {
        if ty.physical() != rudb_common::PhysicalType::Varlen {
            return Err(Error::internal(format!(
                "a {ty} vector cannot use an external text source"
            )));
        }
        let len = source.len();
        Ok(Self { ty, len, validity: Validity::AllValid, body: Body::ExternalText { source } })
    }

    /// The same strings, in a form where a cut of them does not copy the bytes.
    ///
    /// The counterpart of [`Self::run_encoded`] and [`Self::bit_packed`] for a string column, and
    /// the only one of the three that takes `self` by value. It has to: what it does is move the
    /// arena into an `Arc` so nothing copies it again, and a version taking `&self` would start by
    /// copying the arena once to have one to move.
    ///
    /// Anything that is not a flat string column comes back as it was, which includes a column that
    /// is already in this form.
    ///
    /// # Errors
    ///
    /// Nothing here fails today. The result is a `Result` because the check inside
    /// [`Self::string_views`] is worth running on the views this builds rather than trusting that
    /// this function built them right.
    pub fn shared_text(self) -> Result<Self> {
        let Body::Flat(Data::Varlen(column)) = self.body else {
            return Ok(self);
        };
        let (views, arena) = column.into_parts();
        let shared = Self::string_views(self.ty, views, Arc::new(arena))?;
        Ok(shared.with_validity(self.validity))
    }

    /// A vector of FSST codes against a table somebody else trained.
    ///
    /// The way in for a reader that has a page of compressed strings and the table that goes with
    /// it. The codes are not copied and the table is not retrained, so laying several chunks over
    /// one page costs the spans and nothing else.
    ///
    /// # Errors
    ///
    /// If the type is not one stored as text, or if a span runs past the end of the codes.
    pub fn coded(
        ty: LogicalType,
        codes: Arc<Vec<u8>>,
        spans: Vec<(u32, u32)>,
        table: Arc<SymbolTable>,
    ) -> Result<Self> {
        if ty.physical() != rudb_common::PhysicalType::Varlen {
            return Err(Error::internal(format!("a {ty} vector cannot hold FSST codes")));
        }
        let end = u32::try_from(codes.len()).unwrap_or(u32::MAX);
        if spans.iter().any(|&(from, to)| from > to || to > end) {
            return Err(Error::internal("an FSST span runs past the end of the codes"));
        }
        let len = spans.len();
        Ok(Self {
            ty,
            len,
            validity: Validity::AllValid,
            body: Body::Coded { codes, spans, table },
        })
    }

    /// The same strings, compressed against a table trained on them.
    ///
    /// The counterpart of [`Self::run_encoded`] and [`Self::bit_packed`] for a text column, and it
    /// takes `self` by value for the reason [`Self::shared_text`] does.
    ///
    /// The table is trained on every row rather than on a sample. A vector is at most 1024 rows, so
    /// the sample would be most of the column anyway, and the systematic sampling
    /// `spec/06-compression.md` section 6.3 asks for is a decision about a page and belongs to
    /// whoever is holding one.
    ///
    /// It declines unless the codes are at most half the bytes the strings are. FSST gets about that
    /// on text and rather less on anything already short or already random, and below that the
    /// decompression per row read is not bought back. A column it declines on comes back as it was.
    ///
    /// # Errors
    ///
    /// Nothing here fails today. The result is a `Result` because the checks inside [`Self::coded`]
    /// are worth running on what this builds rather than trusting that this built it right.
    pub fn compressed(self) -> Result<Self> {
        let Body::Flat(Data::Varlen(column)) = &self.body else {
            return Ok(self);
        };
        let rows: Vec<&[u8]> = (0..self.len).filter_map(|row| column.bytes(row)).collect();
        if rows.len() != self.len {
            return Ok(self);
        }
        let plain: usize = rows.iter().map(|row| row.len()).sum();
        let table = SymbolTable::train(&rows);
        let mut codes = Vec::with_capacity(plain);
        let mut spans = Vec::with_capacity(self.len);
        for row in &rows {
            let from = u32::try_from(codes.len()).unwrap_or(u32::MAX);
            table.compress(row, &mut codes);
            spans.push((from, u32::try_from(codes.len()).unwrap_or(u32::MAX)));
        }
        if codes.len() * FSST_PAYS_AT > plain {
            return Ok(self);
        }
        let coded = Self::coded(self.ty.clone(), Arc::new(codes), spans, Arc::new(table))?;
        Ok(coded.with_validity(self.validity.clone()))
    }

    /// The same values under a wider decimal type that stores them the same way.
    ///
    /// A decimal is kept as its unscaled integer, so two decimal types with one scale and one
    /// storage width describe the same bits, and going from the narrower of them to the wider is a
    /// relabelling rather than a conversion. The binder writes three of those into
    /// `l_extendedprice * (1 - l_discount)`, because a product's operands are given the answer's
    /// width and the answer's width is eighteen while both columns are fifteen, and each one was a
    /// pass over six million rows that wrote back the bytes it had just read.
    ///
    /// A flat run only, and deliberately. The general cast flattens whatever it is given, so a
    /// dictionary column came out of a width change as a run of values, and a relabelling that kept
    /// the dictionary would hand the arithmetic above two columns it has to read through a code per
    /// row instead of two it can read end to end. That was measured and it is the worse of the two:
    /// on `sum(l_extendedprice * l_discount)` under the filter q6 puts on it, where the rows left
    /// are few and scattered and the indirection is a cache miss each, keeping the dictionary cost
    /// half again as much as the flattening it saved. The flat case has no such question, since
    /// what it hands on is exactly what the pass would have built.
    ///
    /// Only widening, because a narrower width is a range every value has to be checked against and
    /// checking it is the pass this exists to avoid. `None` for anything else, including a narrower
    /// width, a changed scale, a changed storage width and any form but the flat one.
    #[must_use]
    pub fn as_wider_decimal(&self, target: &LogicalType) -> Option<Self> {
        let (
            LogicalType::Decimal { width: from, scale: held },
            LogicalType::Decimal { width: into, scale },
        ) = (&self.ty, target)
        else {
            return None;
        };
        if held != scale || from > into || self.ty.decimal_storage() != target.decimal_storage() {
            return None;
        }
        // Nothing in a flat run says what its numbers mean, so the relabelling is the type and
        // nothing else, and the buffer underneath is shared rather than copied.
        if !matches!(self.body, Body::Flat(_)) {
            return None;
        }
        Some(Self {
            ty: target.clone(),
            len: self.len,
            validity: self.validity.clone(),
            body: self.body.clone(),
        })
    }

    /// The same vector with a different validity.
    #[must_use]
    pub fn with_validity(mut self, validity: Validity) -> Self {
        self.validity = validity;
        self
    }

    /// What kind of values these are.
    #[must_use]
    pub fn logical_type(&self) -> &LogicalType {
        &self.ty
    }

    /// How many values there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many bytes of memory this vector is holding.
    ///
    /// What the memory limit charges for it. A constant and a sequence hold one value and two
    /// numbers however long they are, which is the point of both forms, so the number here is the
    /// form's cost and not the column's width times its length.
    ///
    /// A part that is behind an `Arc` counts as one holder's share of it, which is
    /// [`Buffer::footprint`]'s rule for a shared page applied to the other shared parts. A
    /// dictionary counted in full in every vector sharing it is not a conservative over count, it is
    /// a number with the chunk count in it: an aggregate that emits nineteen thousand chunks of
    /// groups out of one stable dictionary reported that dictionary nineteen thousand times and
    /// refused itself a budget of twenty five gigabytes while the process held one. Dividing by the
    /// holders makes the sum over everything sharing the part come to about the part, which is what
    /// the number is supposed to mean, and it errs high rather than low whenever the holders arrive
    /// one after another, because each of them counts what it sees at the time it asks.
    #[must_use]
    pub fn footprint(&self) -> usize {
        let body = match &self.body {
            Body::Flat(data) => data.footprint(),
            Body::Constant(value) => value.footprint(),
            Body::Sequence { .. } => 0,
            Body::Dictionary { codes, values, .. } => {
                codes.footprint() + share(values.footprint(), values)
            }
            Body::Packed { words, .. } => share(words.capacity() * size_of::<u64>(), words),
            Body::Views { views, arena } => {
                views.capacity() * size_of::<StringView>() + share(arena.footprint(), arena)
            }
            Body::ExternalText { source } => share(source.footprint(), source),
            Body::Coded { codes, spans, table } => {
                share(codes.capacity(), codes)
                    + spans.capacity() * size_of::<(u32, u32)>()
                    + share(table.footprint(), table)
            }
            Body::Runs { ends, values } => {
                ends.capacity() * size_of::<u32>() + share(values.footprint(), values)
            }
            // The ids are shared between every cut of one link join's output, and the source is
            // shared with every other column gathered off the same parent, so both are divided by
            // their holders for the reason the dictionary above is. A gather whose source counted in
            // full would report a parent table per projected column per chunk.
            Body::Gathered { source, rids, .. } => {
                share(rids.capacity() * size_of::<u32>(), rids) + share(source.footprint(), source)
            }
            Body::Nested { entries, child } => {
                entries.capacity() * size_of::<(u32, u32)>() + share(child.footprint(), child)
            }
            // A struct is as wide as its fields are, so this is the one body whose cost is a sum
            // over children rather than one number, and a struct of a hundred narrow fields costs
            // what the hundred columns cost.
            Body::Fields { children } => {
                children.capacity() * size_of::<Arc<Self>>()
                    + children.iter().map(|child| share(child.footprint(), child)).sum::<usize>()
            }
        };
        size_of::<Self>() + self.validity.footprint() + body
    }

    /// Which of the values are not null, at this level and no deeper.
    ///
    /// This is not the same question as [`Self::is_null_at`] and the difference has already cost
    /// one wrong answer. A dictionary and a run length vector keep their nulls in the values they
    /// point at rather than in a mask of their own, so both are built with every row marked present
    /// here and a row whose value is null reads as valid. A caller that wants to know whether a row
    /// is null wants the other one. A caller that wants the mask of a flat column, to copy it or to
    /// count it, wants this one.
    #[must_use]
    pub fn validity(&self) -> &Validity {
        &self.validity
    }

    /// Whether the row at `index` is null, in whichever form the vector is in.
    ///
    /// Reads through a dictionary or a run to the value it stands for, which is where those two
    /// forms keep their nulls, and answers from the mask for every other form. A row past the end
    /// is null, the same answer [`Self::value_at`] gives it.
    #[must_use]
    pub fn is_null_at(&self, index: usize) -> bool {
        if index >= self.len || !self.validity.is_valid(index) {
            return true;
        }
        match &self.body {
            Body::Dictionary { codes, values, .. } => match codes.get(index) {
                Some(&code) => values.is_null_at(code as usize),
                None => true,
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.is_null_at(run),
                None => true,
            },
            // Section 8.2's lazy validity, which is this line. A gather has no mask of its own and
            // does not need one: the id says whether there is a row and the source says whether that
            // row is null, and both of those are already in memory.
            Body::Gathered { source, rids, offset } => match rids.get(offset + index) {
                Some(&NO_ROW) | None => true,
                Some(&rid) => source.is_null_at(rid as usize),
            },
            _ => false,
        }
    }

    /// Whether no row in range is null, answered without reading a row.
    ///
    /// This is the cheap side of [`Self::is_null_at`] and has to follow it exactly. A dictionary and
    /// a run keep their nulls in the values they stand for, so both levels have to say they have
    /// none. Every other form answers from its own mask. A false means only that the cheap answer
    /// was not available, so a caller that gets one still has to ask row by row.
    ///
    /// Public because the alternative a caller has is a pass over the values, and on a dictionary
    /// that is the size of a Parquet column chunk's that pass is the thing it was trying to avoid.
    #[must_use]
    pub fn never_null(&self) -> bool {
        if self.validity.has_nulls(self.len) {
            return false;
        }
        match &self.body {
            Body::Dictionary { values, .. } | Body::Runs { values, .. } => values.never_null(),
            // A gather is never null when no id is the sentinel and the source holds no nulls. The
            // first of those is a pass over the ids rather than a constant, which is the one place
            // this question is not free, and it is worth paying: the ids are four bytes a row and
            // contiguous, and the alternative is reading through to the source once per row for the
            // whole vector, which is the random access this form exists to postpone.
            Body::Gathered { source, rids, offset } => {
                source.never_null()
                    && !rids[*offset..].iter().take(self.len).any(|&rid| rid == NO_ROW)
            }
            _ => true,
        }
    }

    /// Which physical form this vector is in.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.body {
            Body::Flat(_) => Form::Flat,
            Body::Constant(_) => Form::Constant,
            Body::Sequence { .. } => Form::Sequence,
            Body::Dictionary { .. } => Form::Dictionary,
            Body::Packed { .. } => Form::BitPacked,
            Body::Views { .. } => Form::StringView,
            Body::ExternalText { .. } => Form::StringView,
            Body::Coded { .. } => Form::Fsst,
            Body::Runs { .. } => Form::Rle,
            Body::Nested { .. } => Form::List,
            Body::Fields { .. } => Form::Struct,
            Body::Gathered { .. } => Form::Gathered,
        }
    }

    /// The data, for a flat vector, and `None` for any other form.
    ///
    /// A kernel that wants a slice asks for it and takes the flat path if it gets one. A kernel
    /// that can do better on a constant or a dictionary checks [`Self::form`] first.
    #[must_use]
    pub fn data(&self) -> Option<&Data> {
        match &self.body {
            Body::Flat(data) => Some(data),
            _ => None,
        }
    }

    /// The one value, for a constant vector, and `None` for any other form.
    ///
    /// A kernel comparing a column against a literal wants the literal once rather than 1024
    /// times, and [`Self::value_at`] on a constant clones it on every call because it has to be
    /// able to hand back a `Value` for any form. This is the accessor that lets the specialized
    /// path hoist the clone out of the loop.
    #[must_use]
    pub fn constant_value(&self) -> Option<&Value> {
        match &self.body {
            Body::Constant(value) => Some(value.as_ref()),
            _ => None,
        }
    }

    /// The codes and the values, for a dictionary vector, and `None` for any other form.
    ///
    /// The reason a kernel needs this rather than reading the dictionary through
    /// [`Self::value_at`] is the entire argument for the form existing. A filter against a
    /// dictionary column of 1024 rows and 40 distinct values is 40 comparisons and 1024 lookups,
    /// not 1024 comparisons, and there is no way to write that loop without seeing the codes.
    ///
    /// Note what the validity of the returned vector means. A dictionary keeps its nulls in the
    /// vector it points at, and the dictionary's own validity says nothing about them, so a caller
    /// deciding whether row `i` is null has to ask the value vector about `codes[i]` rather than
    /// asking this vector about `i`. [`Self::flatten`] has the same note on it for the same
    /// reason, because getting this wrong is a null that survives being selected and comes out as
    /// a zero.
    #[must_use]
    pub fn dictionary_parts(&self) -> Option<(&[u32], &Self)> {
        match &self.body {
            Body::Dictionary { codes, values, .. } => Some((codes, values.as_ref())),
            _ => None,
        }
    }

    /// The codes and the shared dictionary handle for a dictionary vector.
    ///
    /// Storage readers use the identity of this handle to prove that codes from separate pages
    /// belong to one table-wide dictionary. Kernels that only read values should continue to use
    /// [`Self::dictionary_parts`].
    #[must_use]
    pub fn shared_dictionary_parts(&self) -> Option<(&[u32], &Arc<Self>)> {
        match &self.body {
            Body::Dictionary { codes, values, .. } => Some((codes, values)),
            _ => None,
        }
    }

    /// Stable codes and their shared values, when storage guarantees one code space across pages.
    #[must_use]
    pub fn stable_dictionary_parts(&self) -> Option<(&[u32], &Arc<Self>)> {
        match &self.body {
            Body::Dictionary { codes, values, stable: true } => Some((codes, values)),
            _ => None,
        }
    }

    /// The run ends and the run values, for a run length vector, and `None` for any other form.
    ///
    /// The ends are exclusive and increasing, and there is exactly one value per run, so a kernel
    /// that wants to walk this walks the pairs and never asks which run a row is in. That is the
    /// whole argument for the form: an aggregate over a clustered column is one multiply per run
    /// instead of one add per row, and there is no way to write that loop without seeing the ends.
    ///
    /// The nulls are in the values, the way a dictionary's are, so a caller deciding whether row `i`
    /// is null asks the value vector about the run rather than asking this vector about `i`.
    #[must_use]
    pub fn run_parts(&self) -> Option<(&[u32], &Self)> {
        match &self.body {
            Body::Runs { ends, values } => Some((ends, values.as_ref())),
            _ => None,
        }
    }

    /// Where each row's value is, for the two forms that keep their values somewhere else.
    ///
    /// A dictionary and a run length vector are the same shape seen from a kernel: a run of
    /// positions and a vector to read them out of. The difference is that a dictionary stores the
    /// positions and a run length vector works them out, and a kernel writing `values[at[row]]` does
    /// not care which. So every specialization written against [`Self::dictionary_parts`] covers
    /// both forms by asking this instead, and the day a third form with an indirection arrives it
    /// covers that one too without any of those kernels being reopened.
    ///
    /// The run length side costs an allocation of one position per row and a pass to fill it, which
    /// is the same four bytes a row a dictionary was already carrying and is paid once per kernel
    /// call rather than once per row. That is the price of this being one accessor rather than a
    /// second arm in eighteen kernels, and it is not the last word: a kernel that wants a run at a
    /// time reads [`Self::run_parts`] and pays nothing, which is the specialization this makes it
    /// possible to skip writing until a sweep says it is worth it.
    #[must_use]
    pub fn positions(&self) -> Option<(Cow<'_, [u32]>, &Self)> {
        match &self.body {
            Body::Dictionary { codes, values, .. } => Some((Cow::Borrowed(codes), values.as_ref())),
            Body::Runs { ends, values } => {
                let mut at = Vec::with_capacity(self.len);
                for (run, &stop) in ends.iter().enumerate() {
                    let run = u32::try_from(run).unwrap_or(u32::MAX);
                    at.resize(stop as usize, run);
                }
                Some((Cow::Owned(at), values.as_ref()))
            }
            _ => None,
        }
    }

    /// The bits and what they mean, for a bit packed vector, and `None` for any other form.
    ///
    /// What a kernel needs to stay in code space. A comparison against a literal is the case that
    /// pays: `column > 900` over a column packed from a base of 40 is `code > 860`, which is the
    /// same shift and mask the read was going to do anyway and no unpacking at all, and a literal
    /// outside the packed range answers the whole vector without reading a bit of it. None of that
    /// can be written without seeing the width and the base.
    #[must_use]
    pub fn packed_parts(&self) -> Option<Packed<'_>> {
        match &self.body {
            Body::Packed { words, width, base, offset } => {
                Some(Packed { words, width: *width, base: *base, offset: *offset })
            }
            _ => None,
        }
    }

    /// The views and the arena, for either form that stores strings, and `None` for the rest.
    ///
    /// This is to the two string forms what [`Self::positions`] is to the two forms that point
    /// somewhere else. A flat varchar column owns its arena and a string view column shares one, and
    /// a kernel reading a row wants the view and the bytes either way, so every specialization
    /// written against this covers both forms and neither has to be reopened when a third way of
    /// holding an arena arrives.
    ///
    /// The arena is whatever the long strings live in, which for a column over a page is the page,
    /// including the parts of it no view points at. Only the views say which bytes are a row.
    #[must_use]
    pub fn text_parts(&self) -> Option<(&[StringView], &[u8])> {
        match &self.body {
            Body::Flat(Data::Varlen(column)) => Some((column.views(), column.arena())),
            Body::Views { views, arena } => Some((views, arena)),
            _ => None,
        }
    }

    /// The views and the arena they point into, for a vector of string views and nothing else.
    ///
    /// [`Self::text_parts`] answers the same question for a flat column too, and gives the arena as
    /// bytes. This gives the `Arc`, which is what a caller laying several of these end to end needs
    /// to see that they share one arena and can keep it rather than copying out of it.
    #[must_use]
    pub fn shared_views(&self) -> Option<(&[StringView], &Arc<Buffer<u8>>)> {
        match &self.body {
            Body::Views { views, arena } => Some((views, arena)),
            _ => None,
        }
    }

    /// The codes and the table, for an FSST vector, and `None` for any other form.
    ///
    /// What a kernel needs to stay in code space. An equality filter is the case that pays, and it
    /// pays completely: the literal is compressed once against the same table and after that a row
    /// matches exactly when its code bytes match, because compressing is a function and so is
    /// decompressing. No row is decompressed at all. An ordering comparison cannot do that, since a
    /// symbol code says nothing about where its symbol sorts, so those decompress and say so.
    #[must_use]
    pub fn coded_parts(&self) -> Option<Coded<'_>> {
        match &self.body {
            Body::Coded { codes, spans, table } => Some(Coded { codes, spans, table }),
            _ => None,
        }
    }

    /// The start and the step, for a sequence vector, and `None` for any other form.
    #[must_use]
    pub fn sequence_parts(&self) -> Option<(i64, i64)> {
        match self.body {
            Body::Sequence { start, step } => Some((start, step)),
            _ => None,
        }
    }

    /// The value at `index`, as a single value.
    ///
    /// This is the slow path on purpose. It is what a result set is read out with and what a test
    /// asserts on, and an operator that calls it per row is an operator that has already lost the
    /// argument the vector interface exists to win.
    #[must_use]
    pub fn value_at(&self, index: usize) -> Value {
        if index >= self.len || !self.validity.is_valid(index) {
            return Value::Null;
        }
        match &self.body {
            Body::Constant(value) => value.as_ref().clone(),
            Body::Sequence { start, step } => Value::BigInt(start + step * index as i64),
            Body::Dictionary { codes, values, .. } => match codes.get(index) {
                Some(&code) => values.value_at(code as usize),
                None => Value::Null,
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.value_at(run),
                None => Value::Null,
            },
            // The one read every other reader of this form is: follow the id, and answer null when
            // there is no row to follow. Written out once per reader rather than through a helper
            // because each of them returns a different kind of nothing.
            Body::Gathered { source, rids, offset } => match rids.get(offset + index) {
                Some(&NO_ROW) | None => Value::Null,
                Some(&rid) => source.value_at(rid as usize),
            },
            // One value unpacked into a run of one, so that what a packed value means is decided in
            // the same place a flat one is rather than in a second copy of the type mapping that
            // could drift from it. It allocates, which this path is allowed to do and the typed
            // unpack in `copied` is not, and it is the reason anything about to read a packed
            // column a row at a time should flatten it once instead.
            Body::Packed { words, width, base, offset } => {
                unpack(&self.ty, words, *offset, *width, *base, &[index])
                    .map_or(Value::Null, |data| value_from(&self.ty, &data, 0))
            }
            // The bytes are where the arena has them, and what they are read as is the logical
            // type's business, so this hands the row to the same reader a flat column goes through
            // rather than deciding here that a `BLOB` is a string.
            Body::Views { views, arena } => {
                match views.get(index).and_then(|v| v.bytes_in(arena)) {
                    Some(bytes) => bytes_as(&self.ty, bytes),
                    None => Value::Null,
                }
            }
            Body::ExternalText { source } => source
                .bytes_at(index)
                .ok()
                .flatten()
                .map_or(Value::Null, |bytes| bytes_as(&self.ty, bytes)),
            // One row decompressed on its own, which is the property the form is chosen for. It
            // allocates, which this path is allowed to do, and it is the reason anything about to
            // read a compressed column a row at a time should flatten it once instead.
            Body::Coded { codes, spans, table } => {
                match spans.get(index).and_then(|&(from, to)| {
                    let mut out = Vec::new();
                    table.decompress(codes.get(from as usize..to as usize)?, &mut out).ok()?;
                    Some(out)
                }) {
                    Some(bytes) => bytes_as(&self.ty, &bytes),
                    None => Value::Null,
                }
            }
            // A row's elements are read out of the child one at a time, which is the slow path this
            // whole function is and is why a kernel over a list column reads `list_parts` instead.
            // The element type comes from the child rather than from this vector's type, so a list
            // whose child was built narrower than the column claims still hands back what is in it.
            //
            // A map is stored in this body too, so which value comes out is decided by the logical
            // type rather than by the body. That is the one place the composition shows: the bytes of
            // a map really are the bytes of a list of two field structs, and the only thing that
            // remembers it is a map is the type.
            Body::Nested { entries, child } => match (entries.get(index), &self.ty) {
                (Some(&(start, len)), LogicalType::Map(key, value)) => {
                    let pairs = child.struct_parts().unwrap_or_default();
                    Value::map(
                        key.as_ref().clone(),
                        value.as_ref().clone(),
                        (start..start + len)
                            .filter_map(|at| {
                                let [keys, values] = pairs else { return None };
                                Some((keys.value_at(at as usize), values.value_at(at as usize)))
                            })
                            .collect(),
                    )
                }
                (Some(&(start, len)), _) => Value::List {
                    element: child.ty.clone(),
                    values: (start..start + len).map(|at| child.value_at(at as usize)).collect(),
                },
                (None, _) => Value::Null,
            },
            // One value read out of each child at the same position, which is the slow path this whole
            // function is and is why a kernel over a struct column reads `struct_parts` instead. The
            // names come from this vector's type rather than from the children, because a child is a
            // vector and a vector has no name, and the type is where the field order is written down.
            Body::Fields { children } => Value::Struct(
                fields_of(&self.ty)
                    .iter()
                    .zip(children)
                    .map(|(field, child)| (field.name.clone(), child.value_at(index)))
                    .collect(),
            ),
            Body::Flat(data) => value_from(&self.ty, data, index),
        }
    }

    /// One value of this vector's type, built out of bytes the caller already holds.
    ///
    /// [`try_value_at`](Self::try_value_at) finds the bytes itself, which over a dictionary that
    /// keeps its payload in a file means a read. A caller that swept the values out has the bytes in
    /// hand already and wants nothing from here but the type.
    pub fn value_of(&self, bytes: &[u8]) -> Value {
        bytes_as(&self.ty, bytes)
    }

    /// The value at `index`, preserving storage read and validation failures.
    pub fn try_value_at(&self, index: usize) -> Result<Value> {
        if index >= self.len || !self.validity.is_valid(index) {
            return Ok(Value::Null);
        }
        match &self.body {
            Body::ExternalText { source } => {
                Ok(source.bytes_at(index)?.map_or(Value::Null, |bytes| bytes_as(&self.ty, bytes)))
            }
            Body::Dictionary { codes, values, .. } => match codes.get(index) {
                Some(&code) => values.try_value_at(code as usize),
                None => Ok(Value::Null),
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.try_value_at(run),
                None => Ok(Value::Null),
            },
            Body::Nested { entries, child } => match (entries.get(index), &self.ty) {
                (Some(&(start, len)), LogicalType::Map(key, value)) => {
                    let pairs = child.struct_parts().unwrap_or_default();
                    let [keys, values] = pairs else { return Ok(Value::Null) };
                    let mut entries = Vec::with_capacity(len as usize);
                    for at in start..start + len {
                        entries.push((
                            keys.try_value_at(at as usize)?,
                            values.try_value_at(at as usize)?,
                        ));
                    }
                    Ok(Value::map(key.as_ref().clone(), value.as_ref().clone(), entries))
                }
                (Some(&(start, len)), _) => {
                    let mut values = Vec::with_capacity(len as usize);
                    for at in start..start + len {
                        values.push(child.try_value_at(at as usize)?);
                    }
                    Ok(Value::List { element: child.ty.clone(), values })
                }
                (None, _) => Ok(Value::Null),
            },
            Body::Fields { children } => {
                let mut values = Vec::with_capacity(children.len());
                for (field, child) in fields_of(&self.ty).iter().zip(children) {
                    values.push((field.name.clone(), child.try_value_at(index)?));
                }
                Ok(Value::Struct(values))
            }
            _ => Ok(self.value_at(index)),
        }
    }

    /// The text at `index`, borrowed rather than copied.
    ///
    /// [`Self::value_at`] on a `VARCHAR` column allocates a `String` per call, and a group by that
    /// reads a string column keys on one string per input row. This hands back the bytes where they
    /// already are, so a caller with somewhere to put them does not go to the allocator at all.
    ///
    /// `None` for a null, for an index past the end, for a column that is not `VARCHAR`, and for the
    /// constant and sequence forms, whose values are not stored per position. A caller that gets
    /// `None` has to fall back to [`Self::value_at`], which is correct for all of those.
    #[must_use]
    pub fn text_at(&self, index: usize) -> Option<&str> {
        if self.ty != LogicalType::Varchar || index >= self.len || !self.validity.is_valid(index) {
            return None;
        }
        match &self.body {
            Body::Flat(data) => data.str_at(index),
            Body::Dictionary { codes, values, .. } => {
                values.text_at(usize::try_from(*codes.get(index)?).ok()?)
            }
            Body::Runs { ends, values } => values.text_at(run_holding(ends, index)?),
            Body::Gathered { source, rids, offset } => {
                source.text_at(row_of(rids, *offset, index)?)
            }
            Body::Views { views, arena } => {
                std::str::from_utf8(views.get(index)?.bytes_in(arena)?).ok()
            }
            Body::ExternalText { source } => {
                std::str::from_utf8(source.bytes_at(index).ok().flatten()?).ok()
            }
            _ => None,
        }
    }

    /// The variable length bytes at `index`, borrowed without validating or copying them.
    ///
    /// String data is validated when it enters a vector. Hashing and equality only need its bytes,
    /// so those kernels should not pay for UTF-8 validation again on every read.
    #[must_use]
    pub fn bytes_at(&self, index: usize) -> Option<&[u8]> {
        if index >= self.len || !self.validity.is_valid(index) {
            return None;
        }
        match &self.body {
            Body::Constant(value) => match value.as_ref() {
                Value::Varchar(text) => Some(text.as_bytes()),
                Value::Blob(bytes) => Some(bytes),
                _ => None,
            },
            Body::Dictionary { codes, values, .. } => {
                values.bytes_at(usize::try_from(*codes.get(index)?).ok()?)
            }
            Body::Runs { ends, values } => values.bytes_at(run_holding(ends, index)?),
            Body::Gathered { source, rids, offset } => {
                source.bytes_at(row_of(rids, *offset, index)?)
            }
            Body::Views { views, arena } => views.get(index)?.bytes_in(arena),
            Body::ExternalText { source } => source.bytes_at(index).ok().flatten(),
            Body::Flat(data) => data.bytes_at(index),
            // The same `None` [`Self::text_at`] gives, for the same reason. A compressed row is not
            // anywhere in its plain bytes, so there is nothing here to hand back a borrow of, and a
            // caller that gets `None` goes to `value_at` and gets the row decompressed into a value.
            // A list row is `None` for a nearer reason: it is not bytes at all, and a caller wanting
            // its elements wants [`Self::list_parts`] rather than a borrow of one row.
            Body::Coded { .. }
            | Body::Sequence { .. }
            | Body::Packed { .. }
            | Body::Nested { .. }
            | Body::Fields { .. } => None,
        }
    }

    /// Variable length bytes at `index`, preserving storage read and validation failures.
    pub fn try_bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
        if index >= self.len || !self.validity.is_valid(index) {
            return Ok(None);
        }
        match &self.body {
            Body::Constant(value) => Ok(match value.as_ref() {
                Value::Varchar(text) => Some(text.as_bytes()),
                Value::Blob(bytes) => Some(bytes.as_slice()),
                _ => None,
            }),
            Body::Dictionary { codes, values, .. } => match codes.get(index) {
                Some(&code) => values.try_bytes_at(code as usize),
                None => Ok(None),
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.try_bytes_at(run),
                None => Ok(None),
            },
            Body::Gathered { source, rids, offset } => match row_of(rids, *offset, index) {
                Some(row) => source.try_bytes_at(row),
                None => Ok(None),
            },
            Body::Views { views, arena } => {
                Ok(views.get(index).and_then(|view| view.bytes_in(arena)))
            }
            Body::ExternalText { source } => source.bytes_at(index),
            Body::Flat(data) => Ok(data.bytes_at(index)),
            Body::Coded { .. }
            | Body::Sequence { .. }
            | Body::Packed { .. }
            | Body::Nested { .. }
            | Body::Fields { .. } => Ok(None),
        }
    }

    /// Walks the values from `first` up to at most `limit`, without keeping what it read.
    ///
    /// [`TextSource::sweep`] is what this is for and what the doc on it explains. Everything else
    /// here is the honest fallback: a vector that is not reading text out of a file has its values
    /// already, so there is nothing to avoid keeping, and it hands over one value and lets the
    /// caller come back. The answer is one past the last value visited either way, so the loop that
    /// calls this is the same loop whichever form it got.
    ///
    /// Nulls go the slow way. A source that reads a file holds no validity of its own, so the
    /// vector's own mask is the only thing that knows, and rather than teach the sweep about it the
    /// one form that can have both hands over a value at a time through the reader that checks.
    ///
    /// # Errors
    ///
    /// Whatever reading a value raises, and whatever `body` raises.
    pub fn sweep_text(
        &self,
        first: usize,
        limit: usize,
        body: &mut dyn FnMut(usize, &[u8]) -> Result<()>,
    ) -> Result<usize> {
        let limit = limit.min(self.len);
        if first >= limit {
            return Ok(first);
        }
        if let Body::ExternalText { source } = &self.body {
            if matches!(self.validity, Validity::AllValid) {
                return source.sweep(first, limit, body);
            }
        }
        body(first, self.try_bytes_at(first)?.unwrap_or_default())?;
        Ok(first + 1)
    }

    /// A conservative substring test for the payload block holding `first`.
    ///
    /// Only a file-backed string source with all-valid values can skip a whole block. Every other
    /// form returns true and lets the ordinary sweep decide its values.
    pub fn text_block_might_contain(&self, first: usize, literal: &[u8]) -> Result<bool> {
        match &self.body {
            Body::ExternalText { source } if matches!(self.validity, Validity::AllValid) => {
                source.might_contain(first, literal)
            }
            _ => Ok(true),
        }
    }

    /// The values at `indices`, which rise, without keeping what reading them decoded.
    ///
    /// [`TextSource::visit`] is what this is for. A vector that is not reading text out of a file, or
    /// that has nulls of its own, reads a value at a time through the reader that checks.
    ///
    /// # Errors
    ///
    /// Whatever reading a value raises.
    pub fn try_values_visited(&self, indices: &[usize]) -> Result<Vec<Value>> {
        if let Body::ExternalText { source } = &self.body {
            if matches!(self.validity, Validity::AllValid) {
                let mut out = vec![Value::Null; indices.len()];
                let mut own = |at: usize, bytes: &[u8]| {
                    if indices[at] < self.len {
                        out[at] = bytes_as(&self.ty, bytes);
                    }
                    Ok(())
                };
                source.visit(indices, &mut own)?;
                return Ok(out);
            }
        }
        indices.iter().map(|&index| self.try_value_at(index)).collect()
    }

    /// Variable length byte count at `index`, preserving storage failures.
    pub fn try_bytes_len_at(&self, index: usize) -> Result<Option<usize>> {
        if index >= self.len || !self.validity.is_valid(index) {
            return Ok(None);
        }
        match &self.body {
            Body::Dictionary { codes, values, .. } => match codes.get(index) {
                Some(&code) => values.try_bytes_len_at(code as usize),
                None => Ok(None),
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.try_bytes_len_at(run),
                None => Ok(None),
            },
            Body::ExternalText { source } => source.bytes_len_at(index),
            _ => Ok(self.bytes_at(index).map(<[u8]>::len)),
        }
    }

    /// The byte length of every row, in one call to whatever holds the text, when that is possible.
    ///
    /// `into` is cleared and given one length per row. The answer is whether it was: a vector with
    /// nulls in it,
    /// or one whose text is not read from a [`TextSource`], answers `false` and leaves the caller to
    /// ask a row at a time through [`Self::try_bytes_len_at`], which is right for every shape. The
    /// two shapes taken here are the two a scan of a stored string column hands out, the text itself
    /// and a dictionary of codes over it, and each is one call to the source for the whole vector
    /// rather than a call per row down through this type.
    ///
    /// # Errors
    ///
    /// Whatever reading the lengths out of storage raises.
    pub fn try_bytes_lens(&self, into: &mut Vec<i64>) -> Result<bool> {
        self.lens_through(into, false, |source, indices, into| source.bytes_lens_at(indices, into))
    }

    /// The character length of every row, in one call to whatever holds the text, when that is
    /// possible.
    ///
    /// The same shapes as [`Self::try_bytes_lens`], counting characters rather than bytes, which is
    /// `length` where that one is `strlen`. It goes through [`TextSource::chars_lens_at`] so that a
    /// source reading its text out of a file can keep the counts rather than the text, which is the
    /// difference between a scan of `length` over a stored column holding four bytes a distinct
    /// value and holding every distinct value decoded.
    ///
    /// Unlike that one it answers a vector with nulls too, and a null row gets the count of
    /// whatever its slot points at, so the caller masks the nulls itself. Declining a vector with
    /// nulls sent `length` a row at a time through the bytes, which on a stored column is the path
    /// that keeps every block it reads, so one null in a vector was enough to bring that back.
    ///
    /// # Errors
    ///
    /// Whatever reading the text out of storage raises.
    pub fn try_chars_lens(&self, into: &mut Vec<i64>) -> Result<bool> {
        self.lens_through(into, true, |source, indices, into| source.chars_lens_at(indices, into))
    }

    /// One call to `ask` for every row, over the source this vector reads its text from.
    ///
    /// `false` for a vector whose text does not come from a [`TextSource`], and for a vector with
    /// nulls unless `nulls` says the caller will mask them, for the reasons
    /// [`Self::try_bytes_lens`] gives.
    fn lens_through(
        &self,
        into: &mut Vec<i64>,
        nulls: bool,
        ask: impl Fn(&dyn TextSource, &[u32], &mut Vec<i64>) -> Result<()>,
    ) -> Result<bool> {
        if !nulls && !matches!(self.validity, Validity::AllValid) {
            return Ok(false);
        }
        into.clear();
        match &self.body {
            Body::ExternalText { source } => {
                let Ok(rows) = u32::try_from(self.len) else { return Ok(false) };
                let indices = (0..rows).collect::<Vec<_>>();
                ask(source.as_ref(), &indices, into)?;
                Ok(true)
            }
            Body::Dictionary { codes, values, .. } => match &values.body {
                Body::ExternalText { source } if matches!(values.validity, Validity::AllValid) => {
                    let Some(codes) = codes.get(..self.len) else { return Ok(false) };
                    ask(source.as_ref(), codes, into)?;
                    Ok(true)
                }
                _ => Ok(false),
            },
            _ => Ok(false),
        }
    }

    /// Hands `body` the bytes of every row that is not null, when the text is read from a
    /// [`TextSource`], and answers whether it did.
    ///
    /// The rows come in whatever order the source reads them in, each with its row number, so a
    /// caller that writes an answer per row has to put it back in row order itself. That is the
    /// price of the source seeing the whole vector at once, which is what lets one that decodes its
    /// text a block at a time decode each block once for the call rather than keep every block a
    /// row lands in. See [`TextSource::visit_at`]. The shapes taken are the two a scan of a stored
    /// string column hands out, the text itself and a dictionary of codes over it, and anything
    /// else answers `false` and is read a row at a time through [`Self::try_bytes_at`], which is
    /// right for every shape.
    ///
    /// # Errors
    ///
    /// Whatever reading the text out of storage raises, and whatever `body` raises.
    pub fn try_visit_text(&self, body: &mut dyn FnMut(usize, &[u8]) -> Result<()>) -> Result<bool> {
        let (source, codes) = match &self.body {
            Body::ExternalText { source } => (source, None),
            Body::Dictionary { codes, values, .. } => match &values.body {
                Body::ExternalText { source } if matches!(values.validity, Validity::AllValid) => {
                    let Some(codes) = codes.get(..self.len) else { return Ok(false) };
                    (source, Some(codes))
                }
                _ => return Ok(false),
            },
            _ => return Ok(false),
        };
        let Ok(len) = u32::try_from(self.len) else { return Ok(false) };
        // The rows asked for, which are all of them unless some are null. A null row is left out
        // rather than read, because a row at a time read answers it with no value at all.
        let rows: Option<Vec<u32>> = match &self.validity {
            Validity::AllValid => None,
            Validity::AllInvalid => return Ok(true),
            Validity::Mask(mask) => Some((0..len).filter(|&row| mask.get(row as usize)).collect()),
        };
        let indices = match (codes, &rows) {
            (Some(codes), None) => Cow::Borrowed(codes),
            (Some(codes), Some(rows)) => rows.iter().map(|&row| codes[row as usize]).collect(),
            (None, None) => (0..len).collect(),
            (None, Some(rows)) => Cow::Borrowed(rows.as_slice()),
        };
        source.visit_at(&indices, &mut |at, bytes| {
            let row = rows.as_ref().map_or(at, |rows| rows[at] as usize);
            body(row, bytes)
        })?;
        Ok(true)
    }

    /// How many ranks this vector's values have in sorted order, when whatever holds them knows.
    ///
    /// See [`TextSource::ranks`] for what a rank is and what a source promises by answering with
    /// one. Only a vector whose values come from storage can answer, because only storage is in a
    /// position to have sorted them once and written the answer down.
    #[must_use]
    pub fn ranks(&self) -> Option<usize> {
        match &self.body {
            Body::ExternalText { source } => source.ranks(),
            _ => None,
        }
    }

    /// How the value at `rank` compares against `wanted`. See [`TextSource::compare_rank`].
    pub fn compare_rank(&self, rank: usize, wanted: &[u8]) -> Result<Ordering> {
        match &self.body {
            Body::ExternalText { source } => source.compare_rank(rank, wanted),
            _ => {
                Err(Error::internal("a vector without a sorted order was asked to compare a rank"))
            }
        }
    }

    /// Where `wanted` would go in the sorted order. See [`TextSource::below`].
    ///
    /// # Errors
    ///
    /// If this vector has no sorted order, or if a probe of it fails.
    pub fn below(&self, ranks: usize, wanted: &[u8]) -> Result<(usize, bool)> {
        match &self.body {
            Body::ExternalText { source } => source.below(ranks, wanted),
            _ => Err(Error::internal("a vector without a sorted order was asked for a boundary")),
        }
    }

    /// The position of the value at `rank`. See [`TextSource::code_at_rank`].
    pub fn code_at_rank(&self, rank: usize) -> Result<u32> {
        match &self.body {
            Body::ExternalText { source } => source.code_at_rank(rank),
            _ => Err(Error::internal("a vector without a sorted order was asked for a rank")),
        }
    }

    /// The rank of every value, indexed by position. See [`TextSource::code_ranks`].
    #[must_use]
    pub fn code_ranks(&self) -> Option<&[u32]> {
        match &self.body {
            Body::ExternalText { source } => source.code_ranks(),
            _ => None,
        }
    }

    /// Text at `index`, preserving storage read, validation and UTF-8 failures.
    pub fn try_text_at(&self, index: usize) -> Result<Option<&str>> {
        if self.ty != LogicalType::Varchar {
            return Ok(None);
        }
        self.try_bytes_at(index)?
            .map(|bytes| {
                std::str::from_utf8(bytes).map_err(|error| {
                    Error::conversion(format!("invalid UTF-8 in VARCHAR: {error}"))
                })
            })
            .transpose()
    }

    /// Read every storage-backed value reachable through this vector.
    pub fn validate_external(&self) -> Result<()> {
        match &self.body {
            Body::ExternalText { source } => {
                for index in 0..source.len() {
                    source.bytes_at(index)?;
                }
            }
            Body::Dictionary { codes, values, .. } => {
                if values.reaches_storage() {
                    for &code in codes.iter() {
                        values.try_bytes_at(code as usize)?;
                    }
                }
            }
            Body::Runs { values, .. } | Body::Gathered { source: values, .. } => {
                values.validate_external()?;
            }
            Body::Nested { child, .. } => child.validate_external()?,
            Body::Fields { children } => {
                for child in children {
                    child.validate_external()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether any value of this vector is read from storage when it is asked for.
    ///
    /// A dictionary over values already in memory has nothing that can fail to read, and checking
    /// it a code at a time cost the thread that drains a query about a fifth of a sorted table
    /// copy for no answer at all.
    fn reaches_storage(&self) -> bool {
        match &self.body {
            Body::ExternalText { .. } => true,
            Body::Dictionary { values, .. }
            | Body::Runs { values, .. }
            | Body::Gathered { source: values, .. } => values.reaches_storage(),
            Body::Nested { child, .. } => child.reaches_storage(),
            Body::Fields { children } => children.iter().any(|child| child.reaches_storage()),
            _ => false,
        }
    }

    /// The signed integer at `index`, widened, read without building a [`Value`].
    ///
    /// The integer sibling of [`Self::bytes_at`], and it is here for the same caller. A group by on
    /// an integer column compares one key per input row against the group it probed, and doing that
    /// through [`Self::value_at`] built and dropped a sixty four byte value a row at a time for a
    /// number that was already sitting in the column.
    ///
    /// Widened to `i128` because that is what [`Data::signed_at`] hands back underneath, and one
    /// method that covers every signed width is worth more than five that do not. A caller that
    /// wants a narrower type narrows it, which is a range check against a value in a register.
    ///
    /// The types this answers for are the ones whose flat data is read through `signed_at`, so the
    /// five signed integer widths and the decimal, date, time and timestamp types that are stored
    /// in them. A decimal answers with its unscaled value, which is the number the column holds.
    ///
    /// `None` for a null, for an index past the end, for a column of any other type, and for the
    /// compressed form. Packed integers stay in code space and answer `base + code` directly. A
    /// caller that gets `None` falls back to [`Self::value_at`], which is correct for the remaining
    /// forms.
    #[must_use]
    pub fn signed_at(&self, index: usize) -> Option<i128> {
        if index >= self.len || !self.validity.is_valid(index) {
            return None;
        }
        match &self.body {
            Body::Flat(data) => data.signed_at(index),
            Body::Constant(value) => match value.as_ref() {
                Value::TinyInt(x) => Some(i128::from(*x)),
                Value::SmallInt(x) => Some(i128::from(*x)),
                Value::Integer(x) | Value::Date(x) => Some(i128::from(*x)),
                Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => Some(i128::from(*x)),
                Value::HugeInt(x) | Value::Decimal { unscaled: x, .. } => Some(*x),
                _ => None,
            },
            // The same arithmetic [`Self::value_at`] does on a sequence, so the two agree about a
            // sequence that runs off the end of the width it is stored in.
            Body::Sequence { start, step } => {
                Some(i128::from(start.wrapping_add(step.wrapping_mul(index as i64))))
            }
            Body::Dictionary { codes, values, .. } => {
                values.signed_at(usize::try_from(*codes.get(index)?).ok()?)
            }
            Body::Runs { ends, values } => values.signed_at(run_holding(ends, index)?),
            Body::Gathered { source, rids, offset } => {
                source.signed_at(row_of(rids, *offset, index)?)
            }
            Body::Packed { words, width, base, offset } => Some(
                *base + i128::from(code_at(words, (*offset + index) * *width as usize, *width)),
            ),
            // The same `None` [`Self::bytes_at`] gives, for the same reason. A compressed row is not
            // an integer anywhere until it has been unpacked, and a caller that gets
            // `None` goes to `value_at` and gets the row unpacked into a value. A list row is not an
            // integer in any form, however many integers are in it, and a struct row is not one even
            // when it has exactly one integer field, since the row is the struct and not the field.
            Body::Coded { .. }
            | Body::Views { .. }
            | Body::ExternalText { .. }
            | Body::Nested { .. }
            | Body::Fields { .. } => None,
        }
    }

    /// The rows `at` names, read as signed integers, widened and written into `out`.
    ///
    /// The gathered form of [`Self::signed_block`] for a flat vector, which is what a filter's
    /// selection over a flat integer column wants. `false`, with `out` cleared, for every other
    /// form and for a row past the end, and the caller then goes the way it went before.
    #[must_use]
    pub fn signed_gather(&self, at: &[u32], out: &mut Vec<i64>) -> bool {
        out.clear();
        match &self.body {
            Body::Flat(data) => data.signed_gather(self.len, at, out),
            _ => false,
        }
    }

    /// Every signed value in order, widened to `i64`, written into `out`.
    ///
    /// The bulk form of [`Self::signed_at`], for a caller that is going to read the whole vector
    /// anyway. A group by on two integer columns called `signed_at` once per column per row, and
    /// every one of those matched on the body, called into the data and matched again on the
    /// layout, which is about sixty five instructions to read a number that was already sitting in
    /// a slice. It was a fifth of ClickBench 32 on its own.
    ///
    /// A null writes whatever the body holds under it, which is the zero a flat column keeps behind
    /// its mask. Nulls are a separate question and the caller asks it separately, from
    /// [`Self::none_null`] once for the vector when that answers and a row at a time when it does
    /// not.
    ///
    /// `false`, with `out` left empty, for a vector this cannot hand over as a block: `HUGEINT` and
    /// the wide decimals, whose values do not fit an `i64`, the string and nested forms, the
    /// compressed form, and the run form. A caller that gets `false` reads the vector the way it
    /// read it before, with [`Self::signed_at`].
    ///
    /// A dictionary is read as its entries widened once and then a gather through the codes. That
    /// is the form a Parquet integer column arrives in, because DuckDB writes most of them with a
    /// dictionary, and reading one a row at a time was 4 percent of the CPU of loading the 10m
    /// ClickBench file, all of it in the sieve the writer builds for each part. A dictionary whose
    /// entries hold a null is refused, since the row that points at one is null and the only null
    /// check a caller of this makes on a dictionary may be on its codes.
    #[must_use]
    pub fn signed_block(&self, out: &mut Vec<i64>) -> bool {
        out.clear();
        match &self.body {
            Body::Flat(data) => data.signed_block(self.len, out),
            Body::Constant(value) => {
                let held = match value.as_ref() {
                    Value::TinyInt(x) => i64::from(*x),
                    Value::SmallInt(x) => i64::from(*x),
                    Value::Integer(x) | Value::Date(x) => i64::from(*x),
                    Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => *x,
                    _ => return false,
                };
                out.resize(self.len, held);
                true
            }
            // The same arithmetic [`Self::signed_at`] does on a sequence, once per row rather than
            // once per call, and it wraps where that one wraps.
            Body::Sequence { start, step } => {
                out.extend(
                    (0..self.len).map(|index| start.wrapping_add(step.wrapping_mul(index as i64))),
                );
                true
            }
            Body::Packed { words, width, base, offset } => match i64::try_from(*base) {
                Ok(base) => {
                    out.extend((0..self.len).map(|index| {
                        base.wrapping_add(code_at(
                            words,
                            (*offset + index) * *width as usize,
                            *width,
                        ) as i64)
                    }));
                    true
                }
                Err(_) => false,
            },
            Body::Dictionary { codes, values, .. } => {
                let mut entries = Vec::new();
                if !values.none_null() || !values.signed_block(&mut entries) {
                    return false;
                }
                let Some(codes) = codes.get(..self.len) else {
                    return false;
                };
                out.reserve(codes.len());
                for &code in codes {
                    match entries.get(code as usize) {
                        Some(&entry) => out.push(entry),
                        None => {
                            out.clear();
                            return false;
                        }
                    }
                }
                true
            }
            Body::Runs { .. }
            | Body::Gathered { .. }
            | Body::Coded { .. }
            | Body::Views { .. }
            | Body::ExternalText { .. }
            | Body::Nested { .. }
            | Body::Fields { .. } => false,
        }
    }

    /// Whether the vector holds no nulls at all, asked once rather than a row at a time.
    ///
    /// The bulk form of [`Self::is_null_at`], and it answers the same question that one does, so a
    /// dictionary and a run are read through to the values behind them where those two keep their
    /// nulls. A dictionary that holds a null no code points at answers `false` here and `false` at
    /// every row, which is the safe direction and is the only place the two can differ.
    ///
    /// A caller that gets `false` goes back to asking a row at a time.
    #[must_use]
    pub fn none_null(&self) -> bool {
        if self.validity.has_nulls(self.len) {
            return false;
        }
        match &self.body {
            Body::Dictionary { values, .. } | Body::Runs { values, .. } => values.none_null(),
            Body::Gathered { source, rids, offset } => {
                source.none_null()
                    && !rids[*offset..].iter().take(self.len).any(|&rid| rid == NO_ROW)
            }
            _ => true,
        }
    }

    /// Every value in order, as single values.
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.len).map(|index| self.value_at(index))
    }

    /// This vector with its payload held as a page, so that copying or cutting it is free.
    ///
    /// For a producer that means to hand the same values out many times, which is what a stored
    /// column is. A flat body, a dictionary and a string body are the forms this changes, because
    /// each owns a run a copy would have to copy: the values of a flat body, the codes of a
    /// dictionary and the arena of a string body. The rest come back as they were, because a packed
    /// body shares its words, an FSST body shares its codes and its table, and a constant and a
    /// sequence have nothing to share.
    ///
    /// The string body is the one worth spelling out, because an `Arc` around the arena looks like
    /// sharing and is not the sharing that matters. Every reader that wants a run of an arena
    /// without copying the bytes asks [`Buffer::is_shared`], which is a question about the store
    /// inside the `Arc` and not about the `Arc`: an owned store clones by copying every byte and a
    /// page clones by taking a handle. So an arena that was built rather than read stays a thing
    /// each reader copies out of until somebody calls this, however many `Arc`s point at it. The
    /// reader this is for is [`Self::gather`] over a parent column, which without it copies the
    /// bytes of every gathered string once per chunk.
    ///
    /// Only when the arena is this vector's alone, which is the case a producer that has just built
    /// one is in. An arena with another holder is left as it is, because turning it into a page
    /// behind their back would mean copying it, which is the cost this exists to avoid.
    ///
    /// Not recursive into a nested column's children, because a `LIST` or a `STRUCT` holds its
    /// children behind an `Arc` already.
    #[must_use]
    pub fn into_pages(self) -> Self {
        let body = match self.body {
            Body::Flat(data) => Body::Flat(data.into_pages()),
            Body::Dictionary { codes, values, stable } => {
                Body::Dictionary { codes: codes.into_page(), values, stable }
            }
            Body::Views { views, arena } => Body::Views { views, arena: paged(arena) },
            other => other,
        };
        Self { body, ..self }
    }

    /// A contiguous run of the values, in the form they are already in.
    ///
    /// This is the cut [`Self::gather`] cannot do. A gather walks a dictionary to its leaf and
    /// copies, so gathering a piece of a dictionary encoded column hands back a flat one, and a
    /// caller that only wanted the first thousand rows of a page has silently paid for a copy and
    /// thrown the dictionary away. A group by over a dictionary encoded column is the case that
    /// cares, and it is most of ClickBench.
    ///
    /// So each form is cut as itself. A dictionary keeps its dictionary and slices its codes, a
    /// sequence stays arithmetic with its start moved along, a constant stays a shorter constant,
    /// and a flat body is a window into its page when it has one and a copy of its range when it
    /// does not, which [`Self::into_pages`] is how a producer decides.
    ///
    /// The dictionary itself is shared rather than copied, so a cut is the codes and nothing else.
    /// It used to be copied, and on a read of a ClickBench partition that copy was ten percent of
    /// the cycles: a page holds one dictionary and is cut into chunk sized pieces, so the whole
    /// dictionary was copied once per chunk to be read the same way each time.
    ///
    /// # Errors
    ///
    /// If the range runs past the end of the vector, or if the type has no flat layout and the
    /// body is one that has to be copied.
    pub fn slice(&self, at: usize, len: usize) -> Result<Self> {
        let end = at.checked_add(len).ok_or_else(|| Error::internal("a slice that wraps"))?;
        if end > self.len {
            return Err(Error::internal(format!("rows {at} to {end} of a vector of {}", self.len)));
        }
        if at == 0 && len == self.len {
            return Ok(self.clone());
        }
        let validity = self.validity.slice(at, len);
        let body = match &self.body {
            Body::Constant(value) => Body::Constant(value.clone()),
            Body::Sequence { start, step } => {
                Body::Sequence { start: start + step * at as i64, step: *step }
            }
            Body::Dictionary { codes, values, stable } => Body::Dictionary {
                codes: codes.slice(at, len),
                values: Arc::clone(values),
                stable: *stable,
            },
            // The same cut [`Body::Packed`] below takes and for the same reason, and here it is free
            // rather than merely cheap: a link join fills one buffer of parent rows per child chunk
            // and the pipeline cuts it, so moving the starting row is what keeps the ids from being
            // copied once per cut. Both ends of the gather stay shared, the ids and the source.
            Body::Gathered { source, rids, offset } => Body::Gathered {
                source: Arc::clone(source),
                rids: Arc::clone(rids),
                offset: offset + at,
            },
            // The bits are not byte aligned, so a cut either repacks them or moves the row the
            // reading starts at. Moving it is one addition and repacking is a pass, and a page is
            // cut into chunk sized pieces often enough that the difference is the form.
            Body::Packed { words, width, base, offset } => Body::Packed {
                words: Arc::clone(words),
                width: *width,
                base: *base,
                offset: offset + at,
            },
            // The cut a flat string column cannot do. Sixteen bytes a row move and the payload stays
            // where the page put it, so taking a chunk out of a column of long strings costs the
            // same as taking one out of a column of integers. A flat varchar body copies every byte
            // of every long string in the range instead, which is the measurement written down in
            // `Chunk::compact`: compaction loses on a varchar column, and this is the half of the
            // reason that is about cutting rather than about selecting.
            Body::Views { views, arena } => {
                Body::Views { views: views[at..end].to_vec(), arena: Arc::clone(arena) }
            }
            // The spans are absolute positions in the shared codes, so a cut is a run of them and
            // nothing has to be rebased. One page of compressed strings, one table, and as many
            // chunks over it as the reader wants.
            Body::Coded { codes, spans, table } => Body::Coded {
                codes: Arc::clone(codes),
                spans: spans[at..end].to_vec(),
                table: Arc::clone(table),
            },
            // Only the runs the range touches survive, the first and last of them cut back to where
            // the range starts and stops, and every end moved to be relative to the new row zero. A
            // cut of a hundred rows out of a column of a hundred million is a handful of runs, which
            // is the reason this form is worth cutting as itself rather than copying out.
            Body::Runs { ends, values } if len > 0 => {
                let first = run_holding(ends, at).unwrap_or(0);
                let last = run_holding(ends, end - 1).unwrap_or(first);
                let cut: Vec<u32> = ends[first..=last]
                    .iter()
                    .map(|&stop| stop.min(end as u32) - at as u32)
                    .collect();
                let values = values.slice(first, last - first + 1)?;
                Body::Runs { ends: cut, values: Arc::new(values) }
            }
            // An empty cut has no run to point at and an empty run length body would be a vector of
            // no runs claiming a length, so it comes back as the empty flat vector instead.
            Body::Runs { .. } => return self.gather(&[]),
            // The entries are absolute positions in the shared child, so a cut is a run of them and
            // nothing has to be rebased, the same as a cut of FSST spans. The elements outside the
            // range stay in the child unreferenced, which is the trade this form makes: a chunk cut
            // out of a page of lists moves eight bytes a row and copies no elements at all.
            Body::Nested { entries, child } => {
                Body::Nested { entries: entries[at..end].to_vec(), child: Arc::clone(child) }
            }
            // Every child cut at the same place, because a struct row is one value per field at the
            // same position in each and there is no entry standing between the row and the child to
            // rewrite instead. So this is the one nested form whose cut is not free, and what it costs
            // is whatever cutting each field costs, which for a field of string views is sixteen bytes
            // a row and for a field of packed integers is one addition.
            Body::Fields { children } => Body::Fields {
                children: children
                    .iter()
                    .map(|child| child.slice(at, len).map(Arc::new))
                    .collect::<Result<Vec<_>>>()?,
            },
            Body::ExternalText { source } => {
                let mut out = StringColumn::with_capacity(len);
                for index in at..end {
                    out.push_bytes(source.bytes_at(index)?.unwrap_or_default());
                }
                Body::Flat(Data::Varlen(out))
            }
            // The one form with nowhere to point, so its range is copied out. A run and not a
            // gather: this used to build a vector of the positions `at..end` and hand it to
            // `gather`, which then built a vector of `usize` from it, a vector of `bool` beside
            // that, and read the values back one bounds checked index at a time. That is five
            // passes and three allocations to say `memcpy`, and on a scan it was the largest thing
            // in the program after the aggregation itself, because every chunk of every column of
            // every page comes through here.
            Body::Flat(data) => Body::Flat(run_of(data, at, end)),
        };
        Ok(Self { ty: self.ty.clone(), len, validity, body })
    }

    /// The same values in flat form.
    ///
    /// Flattening a vector that is already flat is free. Flattening any other form costs a copy,
    /// which is exactly why the other forms exist and why nothing on the hot path should call
    /// this. It is here for the operators that genuinely cannot do better and for the tests that
    /// check the other forms against it.
    ///
    /// A call that copies counts itself against [`Cause::Flatten`], because a flatten on a hot path
    /// is the most expensive thing in this crate and the only way to find one is to have the number.
    /// A call on a vector that is already flat does not count, since it neither copies nor gives
    /// anything up.
    ///
    /// # Errors
    ///
    /// If the type is one there is no vector for yet, which today means `ARRAY` and `UNION`. A `LIST`
    /// and a `MAP` flatten to themselves and a `STRUCT` to a struct of flattened fields, since none of
    /// the three has a data slice in any form and there is nothing flatter to become.
    pub fn flatten(&self) -> Result<Self> {
        if let Body::Flat(_) = self.body {
            return Ok(self.clone());
        }
        slow::took(Cause::Flatten);
        if let Some(flat) = self.decoded_codes() {
            return Ok(flat);
        }
        self.copied((0..self.len).collect(), false)
    }

    /// A dictionary with no nulls over flat values with none, written out by its codes.
    ///
    /// The general copy walks the positions down through every layer and marks each one that
    /// lands on a null, and then builds the validity back up from those marks. With no null on
    /// either side the codes are already the positions and the validity is already known, so that
    /// is one pass over the codes rather than four. A Parquet column that was dictionary encoded
    /// comes in as this form, and flattening columns on the way to the file was four percent of a
    /// ClickBench load.
    fn decoded_codes(&self) -> Option<Self> {
        let Body::Dictionary { codes, values, .. } = &self.body else {
            return None;
        };
        if !matches!(self.validity, Validity::AllValid)
            || !matches!(values.validity, Validity::AllValid)
        {
            return None;
        }
        let Body::Flat(data) = &values.body else {
            return None;
        };
        if matches!(data, Data::Empty) {
            return None;
        }
        let codes = codes.as_slice().get(..self.len)?;
        if !below(codes, values.len) {
            return None;
        }
        let at = codes.iter().map(|&code| code as usize).collect::<Vec<_>>();
        Some(Self {
            ty: self.ty.clone(),
            len: self.len,
            validity: Validity::AllValid,
            body: Body::Flat(copy_of(data, &at)),
        })
    }

    /// The same values in flat form, taking the vector rather than borrowing it.
    ///
    /// A vector that is already flat comes back as itself, which is the whole reason this exists
    /// beside [`Self::flatten`]. Flattening through a borrow has to clone that vector, and a clone
    /// of a flat vector that owns its values copies every one of them to produce a vector that is
    /// identical to the one it was handed. Anything not already flat goes the same way it does
    /// through [`Self::flatten`], since the copy is real work there rather than work for nothing.
    ///
    /// # Errors
    ///
    /// The same values flat, for a kernel that has a loop over runs and was handed a form it has
    /// no way to index into.
    ///
    /// This is [`Self::flatten`] without the count against [`Cause::Flatten`], and the difference
    /// is who is calling. A flatten is counted because it is usually a shortcut past a loop nobody
    /// wrote. This is for the caller that has the loop and whose alternative is a `Value` per row,
    /// which costs a good deal more than the copy. ClickBench q40 adds three `SMALLINT` columns out
    /// of Parquet, a packed one and runs over the others after the filter, and every `+` went a
    /// row at a time.
    ///
    /// # Errors
    ///
    /// Whatever the copy raises.
    pub fn opened(&self) -> Result<Self> {
        if let Body::Flat(_) = self.body {
            return Ok(self.clone());
        }
        if let Some(flat) = self.decoded_codes() {
            return Ok(flat);
        }
        self.copied((0..self.len).collect(), false)
    }

    /// The same as [`Self::flatten`].
    pub fn into_flat(self) -> Result<Self> {
        if let Body::Flat(_) = self.body {
            return Ok(self);
        }
        // flatten: the caller asked for flat, and the form that is already flat took the branch
        // above, so this is the one case where the copy is what was wanted rather than a shortcut
        // somebody took instead of reading the column where it lies.
        self.flatten()
    }

    /// The values at the given positions, copied, in a form that does not point back at this vector.
    ///
    /// This is the copying counterpart to [`Self::dictionary`], and the two are the two halves of
    /// the decision `spec/07-execution.md` section 7.1 describes. Which half is right is measured
    /// rather than argued, and [`Chunk::compact`](crate::Chunk::compact) is where the measurement
    /// is written down.
    ///
    /// A dictionary chain is walked to its leaf first and the codes composed on the way down, so the
    /// copy runs once over the data rather than once per level, and a position that is null at any
    /// level comes out null here. The copy is a typed loop per physical layout rather than a `Value`
    /// per row, which is the whole point of it and is what [`Self::flatten`] now goes through too.
    ///
    /// # Errors
    ///
    /// If the type is one there is no vector for yet, which today means `ARRAY` and `UNION`. A `LIST`
    /// and a `MAP` gather by permuting their entries and a `STRUCT` by gathering every field.
    pub fn gather(&self, indices: &[u32]) -> Result<Self> {
        // Straight off the positions a filter handed over, since a gather of a stable dictionary is
        // its codes gathered and nothing else, and widening every position first was a pass and an
        // allocation per filtered chunk of `URL` on ClickBench 28.
        if let Body::Dictionary { codes, values, stable: true } = &self.body {
            let inside = below(indices, codes.len());
            return self.stable_gathered(codes, values, indices, inside, |index| index as usize);
        }
        if let Some(gathered) = self.unpacked_at(indices) {
            return Ok(gathered);
        }
        if let Some(gathered) = self.flat_at(indices) {
            return Ok(gathered);
        }
        self.copied(indices.iter().map(|&index| index as usize).collect(), true)
    }

    /// A gather off a flat run of fixed width values with no nulls, every position inside it.
    ///
    /// That is what a join hands out on both of its sides, and the general copy below made a run of
    /// wide positions, walked them for nulls, made a flag per row and a validity out of the flags
    /// before it moved a value. On q09 at SF1 those passes were about half of the gathers. Here it is
    /// one pass for the range and one for the values, and `None` for anything else.
    fn flat_at(&self, indices: &[u32]) -> Option<Self> {
        let Body::Flat(data) = &self.body else { return None };
        if self.validity.has_nulls(self.len) {
            return None;
        }
        if !below(indices, self.len) {
            return None;
        }
        macro_rules! gathered {
            ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
                match data {
                    $(Data::$variant(values) => {
                        let values = values.as_slice();
                        let out: Vec<$native> =
                            indices.iter().map(|&index| values[index as usize]).collect();
                        Data::$variant(Buffer::from_vec(out))
                    })+
                    Data::Empty | Data::Varlen(_) => return None,
                }
            };
        }
        let data = crate::for_each_layout!(fixed, gathered);
        Some(Self {
            ty: self.ty.clone(),
            len: indices.len(),
            validity: Validity::AllValid,
            body: Body::Flat(data),
        })
    }

    /// A gather off a stable dictionary, which is its codes gathered over the same values.
    ///
    /// Generic over the position type because a filter hands over `u32` positions and a nested
    /// gather hands over `usize` ones, and each is read where it lies rather than widened first.
    fn stable_gathered<T: Copy>(
        &self,
        codes: &Buffer<u32>,
        values: &Arc<Vector>,
        at: &[T],
        inside: bool,
        index: impl Fn(T) -> usize,
    ) -> Result<Self> {
        let rows = at.len();
        // The ordinary case, a column with no nulls and a filter's rows all inside it, in one pass
        // for the range and one for the gather. Every code taken is one of this vector's codes,
        // which were range checked when it was built, so the result is not checked again the way
        // a dictionary from outside is. On q1 the two passes this replaces and the check after
        // them were a tenth of the instructions of the scan.
        if inside && self.never_null() {
            return Ok(Self {
                ty: values.ty.clone(),
                len: rows,
                validity: Validity::AllValid,
                body: Body::Dictionary {
                    codes: at.iter().map(|&at| codes[index(at)]).collect(),
                    values: Arc::clone(values),
                    stable: true,
                },
            });
        }
        // Otherwise the rows past the end and the nulls are found one row at a time. The per row
        // question reads through the dictionary to the value it stands for, which is why the case
        // above answers it for the whole column at once.
        let validity = if self.never_null() && at.iter().all(|&at| index(at) < self.len) {
            Validity::AllValid
        } else {
            Validity::from_iter(rows, |row| {
                at.get(row)
                    .map(|&at| index(at))
                    .is_some_and(|index| index < self.len && !self.is_null_at(index))
            })
        };
        let gathered: Vec<u32> =
            at.iter().map(|&at| codes.get(index(at)).copied().unwrap_or(0)).collect();
        // Every code here is one this vector already held, which was checked against the same
        // values on the way in, or the zero a row past the end is written as. So the only code that
        // can be out of range is that zero over no values at all, and the pass that looks for the
        // largest code is not needed to find it. On ClickBench 28 that pass was four percent of the
        // query, because every filtered chunk of `URL` came through here.
        // Values that are themselves a dictionary are composed through by the constructor, and this
        // skips the constructor, so that shape still goes the checked way.
        if matches!(values.body, Body::Dictionary { .. }) {
            return Ok(
                Self::stable_dictionary(gathered, Arc::clone(values))?.with_validity(validity)
            );
        }
        let highest = (values.is_empty() && !gathered.is_empty()).then_some(0);
        Ok(Self::stable_dictionary_validated(gathered, Arc::clone(values), highest)?
            .with_validity(validity))
    }

    /// A packed column's rows at `indices`, unpacked in bulk into a flat column.
    ///
    /// The general copy reads a packed row a code at a time, which is what [`Packed::codes_at`]
    /// exists to avoid. `None` for anything but a packed column with no nulls, every index in range
    /// and both ends of its range inside an `i64`, which is every packed column of TPC-H.
    fn unpacked_at(&self, indices: &[u32]) -> Option<Self> {
        let Body::Packed { words, width, base, offset } = &self.body else {
            return None;
        };
        if self.validity.has_nulls(self.len) {
            return None;
        }
        if !below(indices, self.len) {
            return None;
        }
        let packed = Packed { words, width: *width, base: *base, offset: *offset };
        let low = i64::try_from(packed.base()).ok()?;
        i64::try_from(packed.ceiling()).ok()?;
        // Every value is between the two ends, which both fit, so the add lands without wrapping
        // and the narrowing below keeps every value, since the layout was chosen to hold them.
        #[expect(clippy::cast_possible_wrap, reason = "a code is below the span, which fits")]
        let value = |code: u64| low.wrapping_add(code as i64);
        #[expect(clippy::cast_possible_truncation, reason = "the layout holds every value")]
        let data = match self.ty.physical() {
            rudb_common::PhysicalType::Int64 => {
                Data::Int64(Buffer::from_vec(packed.values_at(indices, value)))
            }
            rudb_common::PhysicalType::Int32 => {
                Data::Int32(Buffer::from_vec(packed.values_at(indices, |code| value(code) as i32)))
            }
            rudb_common::PhysicalType::Int16 => {
                Data::Int16(Buffer::from_vec(packed.values_at(indices, |code| value(code) as i16)))
            }
            _ => return None,
        };
        Some(Self {
            ty: self.ty.clone(),
            len: indices.len(),
            validity: Validity::AllValid,
            body: Body::Flat(data),
        })
    }

    /// The copy both [`Self::gather`] and [`Self::flatten`] are.
    ///
    /// `forms_stay` is the one thing the two want differently. A gather of a constant is a shorter
    /// constant and copying it out would be a thousand writes of the same value for nothing, and a
    /// gather of string views is a shorter run of views over the same arena rather than a copy of
    /// the bytes. Flattening promises flat form to a caller that is about to read the data slice, so
    /// for that one both of them have to be written out.
    fn copied(&self, at: Vec<usize>, forms_stay: bool) -> Result<Self> {
        let rows = at.len();
        if forms_stay {
            if let Body::Dictionary { codes, values, stable: true } = &self.body {
                let inside = at.iter().max().is_none_or(|&top| top < codes.len());
                return self.stable_gathered(codes, values, &at, inside, |index| index);
            }
        }
        let (at, leaf) = self.resolve(at);
        let live: Vec<bool> = at.iter().map(|&index| index != NOWHERE).collect();
        let validity = Validity::from_run(&live);
        let body = match &leaf.body {
            // The same gather the arm below is, for a type that has no flat layout to be written out
            // into. It goes through the nested builders rather than through a run of data, because they
            // are the one place that knows a row of a list column is a range of a child and a row of a
            // struct column is one position in each of several, and a second copy of that here would
            // be a second thing to keep in step with them.
            Body::Constant(value)
                if matches!(
                    self.ty,
                    LogicalType::List(_) | LogicalType::Struct(_) | LogicalType::Map(_, _)
                ) =>
            {
                if forms_stay && matches!(validity, Validity::AllValid) {
                    return Ok(Self::constant(self.ty.clone(), value.as_ref().clone(), rows));
                }
                let rows: Vec<Value> = at
                    .iter()
                    .map(
                        |&index| {
                            if index == NOWHERE { Value::Null } else { value.as_ref().clone() }
                        },
                    )
                    .collect();
                return Self::from_values(self.ty.clone(), &rows);
            }
            // Every position holds the same value, so the only thing the gather can change is the
            // length and which positions are null. A gather with no null in it is still a constant.
            Body::Constant(value) => {
                if forms_stay && matches!(validity, Validity::AllValid) {
                    return Ok(Self::constant(self.ty.clone(), value.as_ref().clone(), rows));
                }
                let mut data = empty_data_for(&self.ty)?;
                for &index in &at {
                    push_value(&mut data, if index == NOWHERE { &Value::Null } else { value })?;
                }
                Body::Flat(data)
            }
            // A sequence is arithmetic rather than storage, so the gather is the arithmetic done at
            // the positions asked for, and a null writes the zero every other layout writes.
            Body::Sequence { start, step } => Body::Flat(Data::Int64(
                at.iter()
                    .map(|&index| if index == NOWHERE { 0 } else { start + step * index as i64 })
                    .collect(),
            )),
            // A flat body with no values is the untyped null, so every position asked for is null
            // whatever was asked for. Going through the copy would build a run of no values and
            // call it `rows` long, which is a vector whose length and data disagree.
            Body::Flat(Data::Empty) => {
                return Ok(Self::constant(self.ty.clone(), Value::Null, rows));
            }
            Body::Flat(data) => Body::Flat(copy_of(data, &at)),
            // The one form whose copy is arithmetic rather than a move of bytes. It goes through a
            // typed loop per layout the way the flat copy does, because the alternative is a `Value`
            // per row and this is the path a flatten of a scanned column takes.
            Body::Packed { words, width, base, offset } => {
                Body::Flat(unpack(&self.ty, words, *offset, *width, *base, &at)?)
            }
            // A gather keeps the form, which is what makes selecting rows out of a string column
            // cost sixteen bytes a row instead of the bytes of the strings. The arena it shares is
            // the whole arena and not the part the kept rows point at, so a selection that throws
            // most of a page away goes on holding the page. That is the trade the form is: a cut and
            // a filter are cheap and the memory comes back when the last vector over the page goes,
            // and a caller that wants the bytes narrowed asks for a flatten.
            Body::Views { views, arena } if forms_stay => Body::Views {
                views: at
                    .iter()
                    .map(|&index| views.get(index).copied().unwrap_or_else(StringView::empty))
                    .collect(),
                arena: Arc::clone(arena),
            },
            // Flattening promises a data slice, and a flat string column is views over an arena
            // just as this form is, so when the arena is a page the flatten is the views and
            // nothing else. The form is given up, which is what was asked for, and not the sharing,
            // which nobody asked to have given up: a result set of six million strings used to copy
            // every byte of them out of the pages they were already sitting in.
            Body::Views { views, arena } if arena.is_shared() => {
                Body::Flat(Data::Varlen(StringColumn::from_parts(
                    at.iter()
                        .map(|&index| views.get(index).copied().unwrap_or_else(StringView::empty))
                        .collect(),
                    (**arena).clone(),
                )))
            }
            // The arena is this vector's own, so there is nothing to share and the bytes are copied
            // out into an arena of their own. The total is known before any of it is copied, the
            // way the flat copy works it out, so the new arena is one allocation.
            Body::Views { views, arena } => {
                let mut out = StringColumn::with_capacity(at.len());
                out.reserve_bytes(
                    at.iter()
                        .filter_map(|&index| views.get(index))
                        .filter(|view| !view.is_inline())
                        .map(StringView::len)
                        .sum(),
                );
                for &index in &at {
                    let bytes = views.get(index).and_then(|view| view.bytes_in(arena));
                    out.push_bytes(bytes.unwrap_or_default());
                }
                Body::Flat(Data::Varlen(out))
            }
            Body::ExternalText { source } => {
                let mut out = StringColumn::with_capacity(at.len());
                for &index in &at {
                    out.push_bytes(source.bytes_at(index)?.unwrap_or_default());
                }
                Body::Flat(Data::Varlen(out))
            }
            // A gather keeps the form, because the codes do not move and a span survives being put
            // in an order the codes are not in. A position that resolved to nowhere gets the empty
            // span, which decompresses to no bytes, which is the zero every other layout writes.
            Body::Coded { codes, spans, table } if forms_stay => Body::Coded {
                codes: Arc::clone(codes),
                spans: at
                    .iter()
                    .map(|&index| spans.get(index).copied().unwrap_or((0, 0)))
                    .collect(),
                table: Arc::clone(table),
            },
            // Flattening decompresses, which is the price of the data slice it promises. The scratch
            // buffer is reused across rows, so this is one allocation for the whole column rather
            // than one per row the way reading it a value at a time would be.
            Body::Coded { codes, spans, table } => {
                let mut out = StringColumn::with_capacity(at.len());
                let mut scratch = Vec::new();
                for &index in &at {
                    scratch.clear();
                    let span = spans
                        .get(index)
                        .and_then(|&(from, to)| codes.get(from as usize..to as usize));
                    if let Some(span) = span {
                        table.decompress(span, &mut scratch)?;
                    }
                    out.push_bytes(&scratch);
                }
                Body::Flat(Data::Varlen(out))
            }
            // The entries move and the child does not, which is the same trade the string forms
            // make and is why a gather of a list column costs eight bytes a row however long the
            // lists are. A position that resolved to nowhere gets a zero length entry, and the mask
            // already says it is null, so the entry is never read.
            //
            // This arm ignores `forms_stay`, unlike every arm above it, because there is nothing
            // flatter for a list to become. The other forms are all cheaper ways of writing down a
            // column of scalars and flattening gives up the saving to hand back a data slice, and a
            // list has no data slice in any form, so a flatten of one is this and a caller reading it
            // goes through `list_parts` either way.
            Body::Nested { entries, child } => Body::Nested {
                entries: at
                    .iter()
                    .map(|&index| entries.get(index).copied().unwrap_or((0, 0)))
                    .collect(),
                child: Arc::clone(child),
            },
            // Every child gathered at the same positions, for the reason the cut cuts every child:
            // there are no entries to permute instead, so the permutation happens once per field. The
            // positions handed down are the resolved ones, sentinel and all, so a row that resolved to
            // nowhere comes back null in each field as well as null here.
            //
            // `forms_stay` is passed straight through rather than ignored, which is the opposite of
            // what the list arm does, and the difference is real. There is nothing flatter for a list
            // to become, and a struct is only as flat as its fields are, so a flatten of a struct
            // column is a flatten of each field and a caller that asked for data slices gets them.
            Body::Fields { children } => Body::Fields {
                children: children
                    .iter()
                    .map(|child| child.copied(at.clone(), forms_stay).map(Arc::new))
                    .collect::<Result<Vec<_>>>()?,
            },
            // Unreachable, because `resolve` walks past every form that points at another vector
            // and stops at the first body that does not.
            Body::Dictionary { .. } | Body::Runs { .. } | Body::Gathered { .. } => {
                return Err(Error::internal(
                    "a form that points somewhere survived being resolved",
                ));
            }
        };
        Ok(Self { ty: self.ty.clone(), len: rows, validity, body })
    }

    /// Where each wanted position lives in the first body that points nowhere else, and that body.
    ///
    /// A position that is null anywhere on the way down, or past the end of anything on the way
    /// down, comes back as [`NOWHERE`]. That single sentinel is what keeps the copy loop from
    /// carrying a validity mask alongside the positions it is already walking.
    fn resolve(&self, mut at: Vec<usize>) -> (Vec<usize>, &Self) {
        let mut source = self;
        loop {
            for slot in &mut at {
                if *slot >= source.len || !source.validity.is_valid(*slot) {
                    *slot = NOWHERE;
                }
            }
            source = match &source.body {
                Body::Dictionary { codes, values, .. } => {
                    for slot in &mut at {
                        *slot = match codes.get(*slot) {
                            Some(&code) => code as usize,
                            None => NOWHERE,
                        };
                    }
                    values.as_ref()
                }
                // A run length body is a dictionary whose code is worked out from the position
                // rather than stored, so the walk down is the same walk with a search where the
                // lookup was. `NOWHERE` searches for nothing and stays `NOWHERE`.
                Body::Runs { ends, values } => {
                    for slot in &mut at {
                        *slot = run_holding(ends, *slot).unwrap_or(NOWHERE);
                    }
                    values.as_ref()
                }
                // The same walk the dictionary above takes, with the sentinel folded into the one
                // this loop already has. That composition is the whole reason a gather is a body
                // rather than an operator: a filter over the output of a link join selects into the
                // ids and copies nothing, and a gather off a gather is one walk down to whatever is
                // at the bottom rather than two passes over the parent.
                Body::Gathered { source: below, rids, offset } => {
                    for slot in &mut at {
                        *slot = if *slot == NOWHERE {
                            NOWHERE
                        } else {
                            row_of(rids, *offset, *slot).unwrap_or(NOWHERE)
                        };
                    }
                    below.as_ref()
                }
                _ => return (at, source),
            };
        }
    }
}

/// So that a kernel can take its operands as either a list of vectors or a list of references.
///
/// A caller that built a `Vec<Vector>` and a caller whose operands are already somewhere else, in a
/// chunk or in an evaluator's scratch, want the same kernel. Without this the second kind has to
/// clone every operand into a `Vec` to satisfy the signature, and a clone of a vector is a copy of
/// the whole column, so the type would be charging real memory traffic for nothing.
impl AsRef<Vector> for Vector {
    fn as_ref(&self) -> &Vector {
        self
    }
}

/// The bits of a packed vector and what they mean, for a kernel that wants to stay in code space.
///
/// Borrowed from the vector rather than owning anything, so getting one costs nothing and a kernel
/// that finds it cannot use them has given up nothing by asking.
#[derive(Debug, Clone, Copy)]
pub struct Packed<'a> {
    words: &'a [u64],
    width: u32,
    base: i128,
    offset: usize,
}

impl Packed<'_> {
    /// Packed words. A persisted vector also records [`Self::offset`].
    #[must_use]
    pub fn words(&self) -> &[u64] {
        self.words
    }

    /// Bit offset, in rows, of the first value.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// How many bits one code takes, between one and [`PACKED_WIDTH_MAX`].
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// What zero means, so that the value of a row is the base plus its code.
    #[must_use]
    pub fn base(&self) -> i128 {
        self.base
    }

    /// The largest value this vector can be holding, whatever it is actually holding.
    ///
    /// With [`Self::base`] this is the pair a comparison kernel wants first. A literal outside the
    /// two answers every row of the vector the same way, which is a whole chunk decided without a
    /// bit being read, and that is the case a zone map would have caught if there were one here.
    #[must_use]
    pub fn ceiling(&self) -> i128 {
        self.base + i128::from(u64::MAX >> (u64::BITS - self.width))
    }

    /// The code of row `row`, which is its value minus [`Self::base`].
    ///
    /// Out of range rows read as zero rather than panicking, the way every other accessor in this
    /// file answers for a row that is not there.
    ///
    /// Marked inline because every caller that matters is a kernel in another crate reading one code
    /// per row, and thin LTO was leaving it as a call there. On TPC-H SF1 that call was 1.5 percent of
    /// the suite and a tenth of q12.
    #[must_use]
    #[inline]
    pub fn code(&self, row: usize) -> u64 {
        code_at(self.words, (self.offset + row) * self.width as usize, self.width)
    }

    /// Which code a value would have, and `None` for a value this vector cannot be holding.
    ///
    /// The translation a comparison does once per vector so that it does not have to unpack once per
    /// row. `None` is the useful answer rather than a failure: it says the literal is outside the
    /// packed range, so every row compares against it the same way.
    #[must_use]
    pub fn code_of(&self, value: i128) -> Option<u64> {
        u64::try_from(value.checked_sub(self.base)?).ok().filter(|&code| code <= self.mask())
    }

    /// The largest code the width allows.
    fn mask(&self) -> u64 {
        u64::MAX >> (u64::BITS - self.width)
    }

    /// The codes of rows `from` to `from + out.len()`, in one pass over the words.
    ///
    /// [`Self::code`] is a code at a time, and every one of them works out which word it is in, reads
    /// it through a bound, and asks whether it straddles into the next. Sixty four codes of one
    /// width fill exactly that many words and the straddles fall in the same places every time, so a
    /// block of them is unpacked by a loop the width is a constant in, where every shift and every
    /// straddle is known before it runs. On TPC-H q1 the code at a time reads were a third of the
    /// instructions the query ran. The rows before the first whole block and after the last one
    /// still go a code at a time.
    pub fn unpack(&self, from: usize, out: &mut [u64]) {
        let width = self.width as usize;
        let start = self.offset + from;
        let end = start + out.len();
        let first = start.next_multiple_of(64).min(end);
        let mut at = 0;
        for row in start..first {
            out[at] = code_at(self.words, row * width, self.width);
            at += 1;
        }
        let mut row = first;
        while row + 64 <= end {
            let word = row / 64 * width;
            let Some(words) = self.words.get(word..word + width) else { break };
            let Some(Ok(block)) = out.get_mut(at..at + 64).map(<&mut [u64; 64]>::try_from) else {
                break;
            };
            unpack_block(words, self.width, block);
            row += 64;
            at += 64;
        }
        for row in row..end {
            out[at] = code_at(self.words, row * width, self.width);
            at += 1;
        }
    }

    /// The code of each of `rows` rows `at` names, in order.
    ///
    /// A filter's selection names rows close together and in order, so the span they cover is
    /// unpacked whole with [`Self::unpack`] and each row read out of it. Rows spread too far apart
    /// for that to pay are read a code at a time.
    ///
    /// Unpacking a block at a time into a buffer on the stack, and reading each row out of the
    /// block it falls in, keeps less in the cache and was tried. The question of which block a row
    /// is in, asked for every row, cost more than the misses it saved, 40.2 G instructions for ten
    /// runs of q1 against 34.1 G this way.
    pub fn codes_at<M: Fn(usize) -> usize>(&self, at: M, rows: usize) -> Vec<u64> {
        let (mut low, mut high) = (usize::MAX, 0);
        for index in 0..rows {
            let row = at(index);
            low = low.min(row);
            high = high.max(row);
        }
        if rows == 0 || high - low >= rows.saturating_mul(4) {
            return (0..rows).map(|index| self.code(at(index))).collect();
        }
        let mut run = vec![0; high - low + 1];
        self.unpack(low, &mut run);
        (0..rows).map(|index| run[at(index) - low]).collect()
    }

    /// The value of each row `at` names, in order, made from its code by `value`.
    ///
    /// [`Self::codes_at`] for a filter's `u32` positions, with the value made as each row is read
    /// rather than in a second pass over the codes. Three things it did cost more than the reads on
    /// q01, where a filter keeps nearly every row of every packed column. The smallest and largest
    /// position were a scalar compare and move a row, because SSE2 has no unsigned or 64 bit
    /// minimum, and here they are signed 32 bit ones, which it has. The span was a fresh buffer
    /// of zeroes, and here each thread keeps one. And the codes were written out whole before the
    /// values were made from them.
    pub fn values_at<T>(&self, at: &[u32], value: impl Fn(u64) -> T) -> Vec<T> {
        thread_local! {
            static SPAN: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
        }
        let Some((low, high)) = extent(at) else { return Vec::new() };
        let (low, high) = (low as usize, high as usize);
        if high - low >= at.len().saturating_mul(4) {
            return at.iter().map(|&row| value(self.code(row as usize))).collect();
        }
        let span = high - low + 1;
        let gathered = |run: &mut Vec<u64>| {
            if run.len() < span {
                run.resize(span, 0);
            }
            let run = &mut run[..span];
            self.unpack(low, run);
            at.iter().map(|&row| value(run[row as usize - low])).collect()
        };
        SPAN.with(|held| match held.try_borrow_mut() {
            Ok(mut held) => gathered(&mut held),
            Err(_) => gathered(&mut Vec::new()),
        })
    }
}

/// Sixty four codes of `width` bits out of the `width` words that hold them, with the width made a
/// constant so that the loop in [`unpack_width`] has nothing left to work out as it goes.
fn unpack_block(words: &[u64], width: u32, out: &mut [u64; 64]) {
    macro_rules! widths {
        ($($width:literal)*) => {
            match width {
                $($width => unpack_width::<$width>(words, out),)*
                _ => {
                    for (at, code) in out.iter_mut().enumerate() {
                        *code = code_at(words, at * width as usize, width);
                    }
                }
            }
        };
    }
    widths!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32
        33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63);
}

#[inline(always)]
fn unpack_width<const WIDTH: usize>(words: &[u64], out: &mut [u64; 64]) {
    let Ok(words) = <&[u64; WIDTH]>::try_from(&words[..WIDTH]) else { return };
    // Written out sixty four times rather than as a loop, because the compiler kept the loop and
    // with it a shift and a branch on the straddle for every code. Spelled out, the row is a
    // constant in each step, so its word, its shift and whether it straddles are all worked out
    // before the program runs and a code is a shift, an or where it straddles and a mask.
    macro_rules! steps {
        ($($at:literal)*) => {
            $(unpack_step::<WIDTH, $at>(words, out);)*
        };
    }
    steps!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32
        33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63);
}

#[inline(always)]
fn unpack_step<const WIDTH: usize, const AT: usize>(words: &[u64; WIDTH], out: &mut [u64; 64]) {
    let bit = AT * WIDTH;
    let word = bit / 64;
    let shift = bit % 64;
    let mut value = words[word] >> shift;
    if shift + WIDTH > 64 {
        value |= words[word + 1] << (64 - shift);
    }
    out[AT] = value & (u64::MAX >> (64 - WIDTH));
}

/// The widest a packed code is allowed to be.
///
/// Sixty three rather than sixty four so that a mask is `u64::MAX >> (64 - width)` with no shift of
/// a whole word in it, and reading a code is one branch on whether it straddles rather than two. A
/// sixty four bit code saves nothing anyway, since it is the layout it came from.
pub const PACKED_WIDTH_MAX: u32 = 63;

/// How much smaller packing has to be before it is worth the shift and the mask on every read.
///
/// Two, so a column packs when the bits come to half the flat size or less. A column that would save
/// a tenth stays flat, because a tenth of a column is not worth turning every read of it into
/// arithmetic, and the whole argument for the form is that a narrow column saves most of itself.
pub const PACKING_PAYS_AT: usize = 2;

/// How much smaller compressing has to be before it is worth a decompression on every read.
///
/// Two, the same rule packing follows and for the same reason. FSST gets about that on text, so a
/// column of English or of URLs compresses and a column of short codes or of random bytes does not,
/// which is the right answer for both.
pub const FSST_PAYS_AT: usize = 2;

/// The codes of a compressed column and the table they are against.
///
/// Handed out by [`Vector::coded_parts`] so a kernel can work in code space. Nothing here
/// decompresses, which is the point: [`Self::encode`] puts the literal into the same space the rows
/// are already in, and after that an equality test is a byte slice comparison.
#[derive(Debug, Clone, Copy)]
pub struct Coded<'a> {
    codes: &'a [u8],
    spans: &'a [(u32, u32)],
    table: &'a SymbolTable,
}

impl Coded<'_> {
    /// The table every row in this vector is compressed against.
    #[must_use]
    pub fn table(&self) -> &SymbolTable {
        self.table
    }

    /// The code bytes of one row, still compressed.
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[u8]> {
        let &(from, to) = self.spans.get(row)?;
        self.codes.get(from as usize..to as usize)
    }

    /// Some bytes in the code space this vector is in.
    ///
    /// The literal side of an equality filter. Compressing is a function of the table and the bytes,
    /// so two strings compress to the same codes exactly when they are the same string, and an
    /// equality test on the codes is an equality test on the strings with no decompression in it.
    #[must_use]
    pub fn encode(&self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        self.table.compress(bytes, &mut out);
        out
    }
}

/// The first `len` of a run of some narrower signed width, sign extended into `out`.
///
/// Written once and called from the three narrow arms of [`Data::signed_block`], so that the sign
/// extension is one loop the compiler can widen rather than three written out by hand.
fn widen<T: Copy + Into<i64>>(run: &[T], len: usize, out: &mut Vec<i64>) -> bool {
    match run.get(..len) {
        Some(run) => {
            out.extend(run.iter().map(|&x| x.into()));
            true
        }
        None => false,
    }
}

/// The rows `at` of the first `len` of `run`, widened, appended to `out`. The range is checked
/// with a maximum first, because a maximum vectorizes and a check on every read would not.
fn gather_widened<T: Copy + Into<i64>>(
    run: &[T],
    len: usize,
    at: &[u32],
    out: &mut Vec<i64>,
) -> bool {
    let Some(run) = run.get(..len) else {
        return false;
    };
    if at.iter().max().is_some_and(|&top| top as usize >= run.len()) {
        return false;
    }
    out.extend(at.iter().map(|&row| run[row as usize].into()));
    true
}

/// One holder's share of a part that several vectors are reading at the same time.
///
/// The rule [`Buffer::footprint`] already uses for a shared page. Everything holding the part asks
/// this, so what they say between them comes to about what the part costs rather than to the part
/// times the number of them, and the answer is never zero for a part that costs anything, because a
/// caller with a reference is at least one holder.
fn share<T: ?Sized>(bytes: usize, held: &Arc<T>) -> usize {
    bytes / Arc::strong_count(held).max(1)
}

/// How many words hold `len` codes of `width` bits.
fn words_for(len: usize, width: u32) -> usize {
    (len * width as usize).div_ceil(u64::BITS as usize)
}

/// The lowest and highest value a type's layout can hold, and `None` for a type with no integer one.
///
/// This is also the test of whether a type can be packed at all, and it is the only one, so the
/// layouts listed here and the layouts [`pack`] and [`unpack`] know how to walk are the same list
/// from the same macro and cannot drift apart.
fn layout_range(ty: &LogicalType) -> Option<(i128, i128)> {
    use rudb_common::PhysicalType as P;
    macro_rules! ranges {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match ty.physical() {
                $(P::$variant => Some((i128::from(<$native>::MIN), i128::from(<$native>::MAX))),)+
                _ => None,
            }
        };
    }
    crate::for_each_layout!(exact, ranges)
}

/// The bytes the first `len` slots of a run take laid flat, whether the run is owned or a window.
fn flat_bytes(data: &Data, len: usize) -> usize {
    macro_rules! widths {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                Data::Empty => 0,
                $(Data::$variant(_) => len * size_of::<$native>(),)+
            }
        };
    }
    crate::for_each_layout!(all, widths)
}

/// What to subtract before packing, so that the whole code range lands inside the column's type.
///
/// The smallest value in the column is the obvious base and it is the wrong one near the top of a
/// type. [`Vector::packed`] checks the two ends of what the codes could say rather than the values
/// that are actually there, which is one check instead of one per row and is what makes reading a
/// packed column cheap. An `INTEGER` column of a thousand values just under `i32::MAX` needs ten
/// bits, and based at its own smallest value those ten bits could say a number an `INTEGER` cannot
/// hold, so the column was refused and the table would not write at all.
///
/// The base does not have to be the smallest value. Any base works where every code is still
/// non-negative and the widest code the width allows still fits the type, which is `base <= low`,
/// `high - base <= 2^width - 1`, `type low <= base` and `base + 2^width - 1 <= type high` together.
///
/// The largest base meeting all four is the one below, and it exists whenever the values fit the
/// type at all: `high - (2^width - 1) <= low` because that is how the width was chosen, and
/// `type low <= type high - (2^width - 1)` because a width wider than the type's own span is
/// already refused. `None` is for a type with no integer layout, which cannot be packed anyway.
fn packing_base(ty: &LogicalType, low: i128, high: i128, width: u32) -> Option<i128> {
    let (floor, ceiling) = layout_range(ty)?;
    let span = i128::from(u64::MAX >> (64 - width));
    let base = low.min(ceiling - span);
    (base >= floor && base >= high - span).then_some(base)
}

/// The lowest and highest value in the first `len` slots of a run of integer data.
///
/// `None` for data that is not integers, which is what says a column cannot be packed. The null
/// slots are in the span, holding whatever zero was written into them, which
/// [`Vector::bit_packed`] says more about.
fn span_of(data: &Data, len: usize) -> Option<(i128, i128)> {
    macro_rules! spans {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    // In the value's own type and one end at a time, which the compiler turns
                    // into vector compares. Widening each value to `i128` first kept both ends in
                    // register pairs and made this two percent of a ClickBench load.
                    let values = values.as_slice();
                    let values = &values[..len.min(values.len())];
                    let low = values.iter().copied().min()?;
                    let high = values.iter().copied().max()?;
                    Some((i128::from(low), i128::from(high)))
                })+
                _ => None,
            }
        };
    }
    crate::for_each_layout!(exact, spans)
}

/// The first `len` values of a run of integer data, written out as codes of `width` bits from `base`.
fn pack(data: &Data, len: usize, base: i128, width: u32) -> Vec<u64> {
    let mut words = vec![0u64; words_for(len, width)];
    macro_rules! packing {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    for (row, &value) in values.as_slice().iter().take(len).enumerate() {
                        // In range because `base` and `width` came from the span of this same run.
                        let code = u64::try_from(i128::from(value) - base).unwrap_or(0);
                        write_code(&mut words, row * width as usize, width, code);
                    }
                })+
                _ => {}
            }
        };
    }
    crate::for_each_layout!(exact, packing);
    words
}

/// The codes at the given rows, unpacked into the flat layout the type calls for.
///
/// A row of [`NOWHERE`] writes the layout's zero, which is the rule [`copy_of`] follows for the same
/// reason: every layout here is a parallel array to a validity mask, so a null takes a slot.
///
/// # Errors
///
/// If the type has no flat layout, which a packed vector cannot have and which is checked when one
/// is built, so an error here is a bug rather than a caller mistake.
fn unpack(
    ty: &LogicalType,
    words: &[u64],
    offset: usize,
    width: u32,
    base: i128,
    at: &[usize],
) -> Result<Data> {
    let mut out = empty_data_for(ty)?;
    let value_of = |row: usize| {
        if row == NOWHERE {
            return None;
        }
        Some(base + i128::from(code_at(words, (offset + row) * width as usize, width)))
    };
    macro_rules! unpacking {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match &mut out {
                $(Data::$variant(values) => {
                    values.reserve(at.len());
                    for &row in at {
                        // In range because both ends of it were checked when the vector was built.
                        let value = value_of(row)
                            .and_then(|value| <$native>::try_from(value).ok())
                            .unwrap_or($zero);
                        values.push(value);
                    }
                })+
                _ => {
                    return Err(Error::internal(format!(
                        "a {ty} vector was packed, which no integer layout allows"
                    )));
                }
            }
        };
    }
    crate::for_each_layout!(exact, unpacking);
    Ok(out)
}

/// The `width` bits starting at `bit`, low end first.
///
/// Zero for bits past the end of the words, which keeps a read of a row that is not there from
/// panicking and matches what every other accessor here does with one.
#[inline]
fn code_at(words: &[u64], bit: usize, width: u32) -> u64 {
    let word = bit / u64::BITS as usize;
    let shift = (bit % u64::BITS as usize) as u32;
    let mask = u64::MAX >> (u64::BITS - width);
    let low = words.get(word).copied().unwrap_or(0) >> shift;
    let taken = u64::BITS - shift;
    if taken >= width {
        return low & mask;
    }
    // The code straddles two words, and `taken` is under the width here so it is under sixty four,
    // which is what makes the shift below one the hardware will do rather than one it refuses.
    let high = words.get(word + 1).copied().unwrap_or(0) << taken;
    (low | high) & mask
}

/// Writes `width` bits of `code` starting at `bit`, over words that started out zero.
fn write_code(words: &mut [u64], bit: usize, width: u32, code: u64) {
    let word = bit / u64::BITS as usize;
    let shift = (bit % u64::BITS as usize) as u32;
    words[word] |= code << shift;
    let taken = u64::BITS - shift;
    if taken < width {
        words[word + 1] |= code >> taken;
    }
}

/// One level of dictionary out of however many levels were handed to [`Vector::dictionary`].
///
/// Every dictionary in the system is built through that constructor and every one of them comes
/// through here first, so the invariant this maintains is that the vector a dictionary points at is
/// never itself a dictionary that could have been composed away. That makes the work a single `if`
/// rather than a loop: the inner vector was already composed when it was built, so composing the
/// outer codes through it leaves the result no deeper than the inner vector already was.
///
/// The codes are indexed rather than fetched with `get`, because the caller has already walked the
/// whole outer array to check that every code is in range and the inner array is exactly as long as
/// the vector those codes were checked against.
fn compose(codes: Vec<u32>, values: Arc<Vector>) -> (Vec<u32>, Arc<Vector>) {
    // A dictionary carrying a validity of its own is one whose nulls live at this level rather than
    // in the values, which is the one thing composition cannot carry down with it.
    if !matches!(values.validity, Validity::AllValid) {
        return (codes, values);
    }
    let Body::Dictionary { codes: inner, values: leaf, .. } = &values.body else {
        return (codes, values);
    };
    debug_assert!(
        !matches!(leaf.body, Body::Dictionary { .. })
            || !matches!(leaf.validity, Validity::AllValid),
        "a dictionary was stacked on a dictionary without going through the constructor"
    );
    // The leaf is handed on as the handle it already is. Nothing here reads it and nothing here
    // changes it, so the composed dictionary points at the same values the stacked one did and
    // whoever else is holding them keeps holding them. This used to take them out of the `Arc`,
    // which copied the whole leaf whenever anybody else was still reading it, and a scan selecting
    // rows out of a chunk whose column came from a shared page dictionary is exactly that: the page
    // holds the leaf, every chunk cut from the page composes through it, and every one of those
    // cuts copied the page's dictionary. TPC-H q21 does it once per thousand rows of `lineitem`.
    let composed = codes.iter().map(|&code| inner[code as usize]).collect();
    (composed, Arc::clone(leaf))
}

/// How many rows a run has to cover on average before run length encoding is smaller.
///
/// A run costs its value plus the four bytes of its end, so on a four byte column a run of two rows
/// breaks even and a run of three wins. Wider columns win sooner and narrower ones later, and this
/// is the one ratio for all of them because a threshold per width is a table that has to be right
/// nine times rather than once. It is a constant with a name so that the sweep that eventually moves
/// it has something to move.
const RUNS_PAY_AT: usize = 2;

/// A string body's arena as a page, when this is the only holder of it.
///
/// The move out of the `Arc` and back into one is what makes this free: [`Buffer::into_page`] takes
/// the run by value and puts it behind an `Arc` without touching a byte of it, so the whole of this
/// is two allocations of a pointer's worth each however large the arena is.
///
/// An arena somebody else is holding comes back untouched. Paging it would mean copying it, since
/// the other holder's view of it has to go on meaning what it meant, and a copy is what the caller
/// asked to avoid.
fn paged(arena: Arc<Buffer<u8>>) -> Arc<Buffer<u8>> {
    if arena.is_shared() {
        return arena;
    }
    match Arc::try_unwrap(arena) {
        Ok(owned) => Arc::new(owned.into_page()),
        Err(held) => held,
    }
}

/// Which run holds `row`, given ends that are exclusive and increasing.
///
/// A binary search rather than a scan, because the callers that ask this are the ones that are not
/// walking the runs in order: a single value read out of a result set, or a gather at scattered
/// positions. Anything walking in order should be reading [`Vector::run_parts`] instead, which is
/// what the form is for.
fn run_holding(ends: &[u32], row: usize) -> Option<usize> {
    let row = u32::try_from(row).ok()?;
    let run = match ends.binary_search(&row) {
        // The ends are exclusive, so landing exactly on one means the row is the first of the next.
        Ok(at) => at + 1,
        Err(at) => at,
    };
    (run < ends.len()).then_some(run)
}

/// The row each run ends at, for a flat body read alongside the validity that goes with it.
///
/// Two adjacent nulls are one run, because a reader of either gets a null and cannot tell them
/// apart. A null between two equal values is three runs for the same reason, since the null is a
/// value of the column as far as anything reading it is concerned.
///
/// The comparison is per layout rather than per `Value`, which is the whole reason this is a macro.
/// A `Value` a row would allocate a string per row on a `VARCHAR` column and would be the exact
/// defect `cargo xtask rowloop` exists to fail the build on.
fn boundaries(data: &Data, validity: &Validity, len: usize) -> Vec<u32> {
    if len == 0 {
        return Vec::new();
    }
    let breaks = |ends: &mut Vec<u32>, mut differs: Box<dyn FnMut(usize, usize) -> bool + '_>| {
        for row in 1..len {
            let same = match (validity.is_valid(row), validity.is_valid(row - 1)) {
                (false, false) => true,
                (true, true) => !differs(row, row - 1),
                _ => false,
            };
            if !same {
                ends.push(u32::try_from(row).unwrap_or(u32::MAX));
            }
        }
        ends.push(u32::try_from(len).unwrap_or(u32::MAX));
    };
    let mut ends = Vec::new();
    macro_rules! walked {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                // No values at all, so every row is the same null and the column is one run.
                Data::Empty => ends.push(u32::try_from(len).unwrap_or(u32::MAX)),
                $(Data::$variant(values) => {
                    breaks(&mut ends, Box::new(|a, b| values.get(a) != values.get(b)));
                })+
                Data::Varlen(values) => {
                    breaks(&mut ends, Box::new(|a, b| values.bytes(a) != values.bytes(b)));
                }
            }
        };
    }
    crate::for_each_layout!(fixed, walked);
    ends
}

/// The position of a value that is not anywhere, because it is null or out of range.
///
/// `usize::MAX` rather than an `Option<usize>`, because the copy loop's bounds check rejects it for
/// free and an `Option` would put a second branch next to the one already there.
pub(crate) const NOWHERE: usize = usize::MAX;

/// The row id of a row that is not in the source, which reads as null.
///
/// Public because whoever builds a [`Form::Gathered`] vector has to write it, and it is `u32::MAX`
/// for the reason the crate's own offset sentinel is `usize::MAX`: a bounds check the reader is
/// doing anyway rejects it, where an `Option<u32>` would be eight bytes a row instead of four and a
/// second branch beside the one already there. It costs the last row of a four billion row source,
/// which is a source no column in this engine has.
pub const NO_ROW: u32 = u32::MAX;

/// Which source row a gathered row names, and `None` when it names none.
///
/// The `Option` is what every reader of [`Body::Gathered`] that returns an `Option` wants, so the
/// three cases that are all *there is nothing here*, past the end of the ids, the sentinel, and an
/// id that does not fit a `usize`, are collapsed once here rather than three times each.
fn row_of(rids: &[u32], offset: usize, index: usize) -> Option<usize> {
    match rids.get(offset + index) {
        Some(&NO_ROW) | None => None,
        Some(&rid) => Some(rid as usize),
    }
}

/// A run of data copied at the given positions, with a zero wherever the position is [`NOWHERE`].
///
/// A zero and not a skip, because every layout here is a parallel array to a validity mask and a
/// short one would put every value after the first null at the wrong index. It is the same rule
/// [`push_value`] follows for a null.
/// A contiguous run of a flat body, copied out.
///
/// The counterpart to [`copy_of`] for the one case that is a range rather than a set of positions,
/// which is what [`Vector::slice`] asks for. Every fixed width layout is one `memcpy` and the
/// string layout is a run of views and their bytes, where `copy_of` is a bounds checked index and a
/// null test per row.
///
/// The caller has already checked that `end` is inside the vector, and a body whose data is shorter
/// than its vector claims is a bug elsewhere, so a short run is clamped rather than reported.
///
/// A fixed width run over a buffer that is a window into a page does not copy anything, because
/// [`Buffer::slice`] moves the offset instead. That is the case a scan over stored memory is in, and
/// it is why the flat body is no longer the one form of a vector whose cut costs an allocation.
fn run_of(data: &Data, at: usize, end: usize) -> Data {
    macro_rules! run {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                Data::Empty => Data::Empty,
                $(Data::$variant(values) => {
                    let held = values.len();
                    let from = at.min(held);
                    let to = end.max(from).min(held);
                    if to == end {
                        // The whole run is there, so this is a window on a shared page and a copy on
                        // an owned one, decided inside the buffer rather than here.
                        Data::$variant(values.slice(from, end - from))
                    } else {
                        let values = values.as_slice();
                        let mut out = Buffer::with_capacity(end - at);
                        out.extend_from_slice(&values[from..to]);
                        // A body shorter than the rows asked for pads with the zero every layout
                        // uses for a null, which is the answer `copy_of` gives for a position past
                        // the end.
                        // row at a time: never runs on a vector whose data matches its length.
                        for _ in to..end {
                            out.push($zero);
                        }
                        Data::$variant(out)
                    }
                })+
                // A view says where its bytes are, so a run of rows is not a run of bytes and this
                // is the one layout whose cut is still a loop. The total is known before any of it
                // is copied, so the arena is one allocation.
                //
                // Unless the payload is a page, in which case the cut points at the same page the
                // column does and no byte of it moves. That is the case a scan of a stored column
                // is in, and it is the whole of why a producer pages its payload: a page cut into
                // chunk sized pieces used to copy every byte of every long string once per piece.
                Data::Varlen(values) => {
                    if let Some(shared) =
                        values.window(at, end).or_else(|| values.viewing(at..end))
                    {
                        return Data::Varlen(shared);
                    }
                    let views = values.views();
                    let mut out = StringColumn::with_capacity(end - at);
                    out.reserve_bytes(
                        views
                            .get(at.min(views.len())..end.min(views.len()))
                            .unwrap_or(&[])
                            .iter()
                            .filter(|view| !view.is_inline())
                            .map(StringView::len)
                            .sum(),
                    );
                    // row at a time: see above, the bytes of consecutive rows need not be next to
                    // each other.
                    for index in at..end {
                        out.push_from(values, index);
                    }
                    Data::Varlen(out)
                }
            }
        };
    }
    crate::for_each_layout!(fixed, run)
}

/// The values of `data` written to the places `inverse` gives them, the other way round from
/// [`copy_of`]: value `n` lands at `inverse[n]`.
///
/// `inverse` is a permutation of the positions of `data` and the answer is as long as it. A place
/// past the end is dropped rather than trusted, and a place nobody wrote keeps the zero, the same
/// zero a gather writes for a position that resolved to nowhere. Strings are turned back into
/// positions and gathered, because their one caller moves the views itself and never sends them.
pub(crate) fn placed_of(data: &Data, inverse: &[u32]) -> Data {
    macro_rules! placed {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    let mut out: Vec<$native> = vec![$zero; inverse.len()];
                    for (value, &to) in values.as_slice().iter().zip(inverse) {
                        if let Some(slot) = out.get_mut(to as usize) {
                            *slot = *value;
                        }
                    }
                    Data::$variant(Buffer::from_vec(out))
                })+
                Data::Empty => Data::Empty,
                // Turned back round into positions and gathered, so a caller that does hand this
                // strings gets the right answer rather than a missing arm.
                Data::Varlen(_) => {
                    let mut at = vec![NOWHERE; inverse.len()];
                    for (row, &to) in inverse.iter().enumerate() {
                        if let Some(slot) = at.get_mut(to as usize) {
                            *slot = row;
                        }
                    }
                    copy_of(data, &at)
                }
            }
        };
    }
    crate::for_each_layout!(fixed, placed)
}

pub(crate) fn copy_of(data: &Data, at: &[usize]) -> Data {
    macro_rules! copied {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                Data::Empty => Data::Empty,
                $(Data::$variant(values) => {
                    let values = values.as_slice();
                    // Into a `Vec` and then into a buffer, rather than pushing at the buffer. A
                    // push asks the buffer whether it owns its run and copies the page out if it
                    // does not, which is the copy on write point and is the right answer for a
                    // caller writing one value. This caller is writing `at.len()` of them into a
                    // run it made itself one line earlier, so the question has one answer and it
                    // is asked once by not being asked at all. The map is exact sized, so the
                    // extend reserves once and writes without a capacity check per value.
                    let mut out: Vec<$native> = Vec::with_capacity(at.len());
                    // One bounds check rather than a null test and a bounds check, because
                    // `NOWHERE` is past the end of every slice there can be.
                    out.extend(at.iter().map(|&index| values.get(index).copied().unwrap_or($zero)));
                    Data::$variant(Buffer::from_vec(out))
                })+
                // The one layout where a gather is a copy of bytes rather than a copy of fixed
                // width slots, and the reason compaction is a decision rather than a default on a
                // string column. A payload that is a page is the exception: the gathered views
                // point at the page the column already points at, so the gather is sixteen bytes a
                // row and the bytes stay where the page put them.
                Data::Varlen(values) => {
                    if let Some(shared) = values.viewing(at.iter().copied()) {
                        return Data::Varlen(shared);
                    }
                    let mut out = StringColumn::with_capacity(at.len());
                    // The bytes are known before any of them are copied, because a view carries its
                    // length and the wanted positions are already in hand, so the arena is one
                    // allocation rather than a run of doublings that each copy what the last one
                    // copied.
                    let views = values.views();
                    out.reserve_bytes(
                        at.iter()
                            .filter_map(|&index| views.get(index))
                            .filter(|view| !view.is_inline())
                            .map(StringView::len)
                            .sum(),
                    );
                    for &index in at {
                        out.push_from(values, index);
                    }
                    Data::Varlen(out)
                }
            }
        };
    }
    crate::for_each_layout!(fixed, copied)
}

/// The physical layout a run of data is in, for the check that it matches its type.
///
/// The two enums name their variants the same way on purpose, so this is one generated arm rather
/// than sixteen chances to pair the wrong two up.
pub(crate) fn layout_of(data: &Data) -> rudb_common::PhysicalType {
    use rudb_common::PhysicalType as P;
    macro_rules! layouts {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                Data::Empty => P::Empty,
                $(Data::$variant(_) => P::$variant,)+
            }
        };
    }
    crate::for_each_layout!(all, layouts)
}

/// One value out of a run of data, given what the run means.
///
/// The match is on the logical type rather than on the data, because the data cannot tell a `DATE`
/// from an `INTEGER` and that is the whole reason the two are kept apart.
fn value_from(ty: &LogicalType, data: &Data, index: usize) -> Value {
    let signed = || data.signed_at(index);
    let unsigned = || data.unsigned_at(index);
    let value = match ty {
        LogicalType::Boolean => match data {
            Data::Bool(v) => v.get(index).map(|&x| Value::Boolean(x)),
            _ => None,
        },
        LogicalType::TinyInt => signed().and_then(|x| i8::try_from(x).ok()).map(Value::TinyInt),
        LogicalType::SmallInt => signed().and_then(|x| i16::try_from(x).ok()).map(Value::SmallInt),
        LogicalType::Integer => signed().and_then(|x| i32::try_from(x).ok()).map(Value::Integer),
        LogicalType::BigInt => signed().and_then(|x| i64::try_from(x).ok()).map(Value::BigInt),
        LogicalType::HugeInt => signed().map(Value::HugeInt),
        LogicalType::UTinyInt => unsigned().and_then(|x| u8::try_from(x).ok()).map(Value::UTinyInt),
        LogicalType::USmallInt => {
            unsigned().and_then(|x| u16::try_from(x).ok()).map(Value::USmallInt)
        }
        LogicalType::UInteger => {
            unsigned().and_then(|x| u32::try_from(x).ok()).map(Value::UInteger)
        }
        LogicalType::UBigInt => unsigned().and_then(|x| u64::try_from(x).ok()).map(Value::UBigInt),
        LogicalType::UHugeInt => unsigned().map(Value::UHugeInt),
        LogicalType::Float => match data {
            Data::Float32(v) => v.get(index).map(|&x| Value::Float(x)),
            _ => None,
        },
        LogicalType::Double => match data {
            Data::Float64(v) => v.get(index).map(|&x| Value::Double(x)),
            _ => None,
        },
        LogicalType::Decimal { width, scale } => {
            signed().map(|unscaled| Value::Decimal { unscaled, width: *width, scale: *scale })
        }
        LogicalType::Varchar | LogicalType::Blob | LogicalType::Bit => {
            data.bytes_at(index).map(|bytes| bytes_as(ty, bytes))
        }
        LogicalType::Date => signed().and_then(|x| i32::try_from(x).ok()).map(Value::Date),
        LogicalType::Time => signed().and_then(|x| i64::try_from(x).ok()).map(Value::Time),
        LogicalType::TimeTz => signed().and_then(|x| i64::try_from(x).ok()).map(Value::TimeTz),
        LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs => {
            signed().and_then(|x| i64::try_from(x).ok()).map(Value::Timestamp)
        }
        LogicalType::TimestampTz => {
            signed().and_then(|x| i64::try_from(x).ok()).map(Value::TimestampTz)
        }
        LogicalType::Interval => match data {
            Data::Interval(v) => {
                v.get(index).map(|&(months, days, micros)| Value::Interval { months, days, micros })
            }
            _ => None,
        },
        _ => None,
    };
    value.unwrap_or(Value::Null)
}

/// The fields a struct type names, and nothing for any other type.
///
/// Only a `STRUCT` vector has a [`Body::Fields`] body, and the two are built together, so in practice
/// the empty slice is unreachable and is here so that reading a field name is not a panic if that ever
/// stops being true. A struct vector whose type has fewer fields than it has children answers about
/// the fields it can name, because the zip stops at the shorter of the two.
fn fields_of(ty: &LogicalType) -> &[Field] {
    match ty {
        LogicalType::Struct(fields) => fields,
        _ => &[],
    }
}

/// One row of a string column as a value, given what its bytes are meant to be read as.
///
/// Both forms that hold strings come through here, so a row that is a `BLOB` in a flat column is a
/// `BLOB` in a string view column too. Bytes that are not text in a `VARCHAR` column are a null
/// rather than a panic, since everything that got in went in as a string and a column that has
/// something else in it is a bug somewhere earlier that a read should not turn into a crash.
fn bytes_as(ty: &LogicalType, bytes: &[u8]) -> Value {
    match ty {
        LogicalType::Varchar => {
            std::str::from_utf8(bytes).map_or(Value::Null, |text| Value::Varchar(text.to_owned()))
        }
        LogicalType::Blob | LogicalType::Bit => Value::Blob(bytes.to_vec()),
        _ => Value::Null,
    }
}

/// An empty run of data of the right layout for a type.
pub(crate) fn empty_data_for(ty: &LogicalType) -> Result<Data> {
    use rudb_common::PhysicalType as P;
    macro_rules! empties {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match ty.physical() {
                P::Empty => Data::Empty,
                $(P::$variant => Data::$variant(Buffer::new()),)+
                P::Varlen => Data::Varlen(StringColumn::new()),
                other => {
                    return Err(Error::not_implemented(format!(
                        "a flat vector of {other:?} data, which arrives with the storage layer"
                    )));
                }
            }
        };
    }
    Ok(crate::for_each_layout!(fixed, empties))
}

/// An empty run of the type's layout with room for `rows` values already taken.
///
/// For a caller that knows how many values are going in before the first one does, which is a
/// producer laying pieces end to end. Growing from empty instead reallocates once per doubling and
/// finishes holding a run rounded up to the next power of two, and on a row group of 122,880 values
/// that rounding is the last 8,192 of them carried for the life of the table.
///
/// Bytes are not reserved for a varlen run, because how many of them there are is not the number of
/// rows and the caller appending them is the one that can work it out.
///
/// # Errors
///
/// If the type has no flat layout, the same as [`empty_data_for`].
pub(crate) fn data_for(ty: &LogicalType, rows: usize) -> Result<Data> {
    let mut data = empty_data_for(ty)?;
    macro_rules! reserved {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match &mut data {
                Data::Empty => {}
                $(Data::$variant(values) => values.reserve(rows),)+
                Data::Varlen(values) => values.reserve_views(rows),
            }
        };
    }
    crate::for_each_layout!(fixed, reserved);
    Ok(data)
}

/// Appends one value to a run of data, or a zero of the right shape when it is null.
///
/// The zero matters. A null still occupies a position, the validity mask is what says it is null,
/// and a run of data with a hole in it would put every value after the hole in the wrong place.
fn push_value(data: &mut Data, value: &Value) -> Result<()> {
    macro_rules! push {
        ($vec:expr, $variant:path, $zero:expr) => {
            match value {
                Value::Null => $vec.push($zero),
                $variant(x) => $vec.push(*x),
                other => {
                    return Err(Error::internal(format!(
                        "{other:?} does not belong in this vector"
                    )));
                }
            }
        };
    }
    // A decimal is stored as its unscaled integer in whatever width its precision needs, which
    // `LogicalType::physical` decides and which is why the same `Value::Decimal` is at home in four
    // different runs. The narrowing cannot fail for a value the binder produced, because the width
    // that chose the run is the width in the value, but it is checked rather than assumed because
    // an unchecked cast here would silently store a different number.
    macro_rules! decimal {
        ($vec:expr, $ty:ty, $unscaled:expr) => {
            match <$ty>::try_from(*$unscaled) {
                Ok(x) => $vec.push(x),
                Err(_) => {
                    return Err(Error::internal(format!(
                        "an unscaled decimal of {} does not fit the run its precision chose",
                        $unscaled
                    )));
                }
            }
        };
    }
    match data {
        Data::Empty => {}
        Data::Bool(v) => push!(v, Value::Boolean, false),
        Data::Int8(v) => push!(v, Value::TinyInt, 0),
        Data::Int16(v) => match value {
            Value::Null => v.push(0),
            Value::SmallInt(x) => v.push(*x),
            Value::Decimal { unscaled, .. } => decimal!(v, i16, unscaled),
            other => return Err(Error::internal(format!("{other:?} is not a 16 bit value"))),
        },
        Data::Int32(v) => match value {
            Value::Null => v.push(0),
            Value::Integer(x) | Value::Date(x) => v.push(*x),
            Value::Decimal { unscaled, .. } => decimal!(v, i32, unscaled),
            other => return Err(Error::internal(format!("{other:?} is not a 32 bit value"))),
        },
        Data::Int64(v) => match value {
            Value::Null => v.push(0),
            Value::BigInt(x)
            | Value::Time(x)
            | Value::TimeTz(x)
            | Value::Timestamp(x)
            | Value::TimestampTz(x) => v.push(*x),
            Value::Decimal { unscaled, .. } => decimal!(v, i64, unscaled),
            other => return Err(Error::internal(format!("{other:?} is not a 64 bit value"))),
        },
        Data::Int128(v) => match value {
            Value::Null => v.push(0),
            Value::HugeInt(x) => v.push(*x),
            Value::Decimal { unscaled, .. } => v.push(*unscaled),
            other => return Err(Error::internal(format!("{other:?} is not a 128 bit value"))),
        },
        Data::UInt8(v) => push!(v, Value::UTinyInt, 0),
        Data::UInt16(v) => push!(v, Value::USmallInt, 0),
        Data::UInt32(v) => push!(v, Value::UInteger, 0),
        Data::UInt64(v) => push!(v, Value::UBigInt, 0),
        Data::UInt128(v) => push!(v, Value::UHugeInt, 0),
        Data::Float32(v) => push!(v, Value::Float, 0.0),
        Data::Float64(v) => push!(v, Value::Double, 0.0),
        Data::Interval(v) => match value {
            Value::Null => v.push((0, 0, 0)),
            Value::Interval { months, days, micros } => v.push((*months, *days, *micros)),
            other => return Err(Error::internal(format!("{other:?} is not an interval"))),
        },
        Data::Varlen(column) => match value {
            Value::Null => {
                column.push("");
            }
            Value::Varchar(text) => {
                column.push(text);
            }
            // A blob goes in as the bytes it is. The column stores a length and some bytes either
            // way, so text is the reading of one rather than a different column, and a blob that
            // is not UTF-8 is stored exactly like one that happens to be.
            Value::Blob(bytes) => {
                column.push_bytes(bytes);
            }
            other => return Err(Error::internal(format!("{other:?} is not a string"))),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::{Field, LogicalType, Value};

    use super::{
        Body, Data, FSST_PAYS_AT, Form, MAP_KEY, MAP_VALUE, NO_ROW, VECTOR_SIZE, Vector, below,
        packing_base,
    };
    use crate::buffer::Buffer;
    use crate::fsst::SymbolTable;
    use crate::string::{StringColumn, StringView};
    use crate::validity::Validity;

    fn integers(values: &[i32]) -> Vector {
        Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into())).unwrap()
    }

    #[test]
    fn below_agrees_with_the_largest_code_whether_the_or_settles_it_or_not() {
        let cases: [(&[u32], usize); 8] = [
            (&[], 0),
            (&[], 5),
            (&[0, 1, 8191], 8192),
            (&[0, 8192], 8192),
            // The `or` of 4 and 1 is 5, which is not below 5, so these take the maximum.
            (&[4, 1], 5),
            (&[4, 5], 5),
            (&[3, 4, 2], 5),
            (&[7], 7),
        ];
        for (codes, len) in cases {
            let expected = codes.iter().all(|&code| (code as usize) < len);
            assert_eq!(below(codes, len), expected, "{codes:?} below {len}");
        }
    }

    #[test]
    fn flattening_a_dictionary_by_its_codes_matches_the_general_copy() {
        let words = Vector::from_values(
            LogicalType::Varchar,
            &["alpha", "a string past the inline length", ""]
                .map(|text| Value::Varchar(text.into())),
        )
        .unwrap();
        let codes = vec![2, 0, 1, 1, 0, 2, 1];
        let cases = [
            Vector::dictionary(codes.clone(), integers(&[7, -3, 40])).unwrap(),
            Vector::dictionary(codes.clone(), words.clone()).unwrap(),
            Vector::dictionary(codes.clone(), words.clone()).unwrap().slice(2, 4).unwrap(),
            // The ones the codes cannot answer alone, which take the general copy.
            Vector::dictionary(codes.clone(), words.clone())
                .unwrap()
                .with_validity(Validity::from_run(&[true, false, true, true, true, true, false])),
            Vector::dictionary(
                vec![0, 1, 1],
                integers(&[1, 2]).with_validity(Validity::from_run(&[true, false])),
            )
            .unwrap(),
        ];
        for (case, vector) in cases.iter().enumerate() {
            let flat = vector.flatten().unwrap();
            let general = vector.copied((0..vector.len()).collect(), false).unwrap();
            assert!(matches!(flat.body, Body::Flat(_)), "case {case}");
            assert_eq!(flat.validity, general.validity, "case {case}");
            for row in 0..vector.len() {
                assert_eq!(flat.value_at(row), general.value_at(row), "case {case} row {row}");
            }
            assert_eq!(flat, vector.opened().unwrap(), "case {case}");
        }
    }

    #[test]
    fn extent_keeps_the_unsigned_order_across_the_sign_bit() {
        assert_eq!(super::extent(&[]), None);
        assert_eq!(super::extent(&[7]), Some((7, 7)));
        let rows = [0x8000_0000, 3, u32::MAX, 0x7fff_ffff, 9];
        assert_eq!(super::extent(&rows), Some((3, u32::MAX)));
    }

    #[test]
    fn unpacking_in_bulk_reads_what_a_code_at_a_time_reads_at_every_width() {
        let mut state = 0x5eed_0b17_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let words: Vec<u64> = (0..700).map(|_| next()).collect();
        for width in 1..=super::PACKED_WIDTH_MAX {
            for offset in [0, 1, 63, 64, 65] {
                let packed = super::Packed { words: &words, width, base: 0, offset };
                for (from, rows) in [(0, 0), (0, 1), (0, 64), (3, 200), (61, 130), (128, 512)] {
                    let mut out = vec![u64::MAX; rows];
                    packed.unpack(from, &mut out);
                    let want: Vec<u64> = (from..from + rows).map(|row| packed.code(row)).collect();
                    assert_eq!(out, want, "width {width} offset {offset} from {from}");
                }
                let at = [5_usize, 9, 9, 70, 6, 200, 131];
                let want: Vec<u64> = at.iter().map(|&row| packed.code(row)).collect();
                assert_eq!(packed.codes_at(|index| at[index], at.len()), want);
                let far = [0_usize, 5000];
                let want: Vec<u64> = far.iter().map(|&row| packed.code(row)).collect();
                assert_eq!(packed.codes_at(|index| far[index], far.len()), want);
                for rows in [&[][..], &[5, 9, 9, 70, 6, 200, 131], &[0, 5000], &[3, 4, 5, 6]] {
                    let want: Vec<u64> =
                        rows.iter().map(|&row| packed.code(row as usize)).collect();
                    assert_eq!(packed.values_at(rows, |code| code), want, "width {width}");
                }
            }
        }
    }

    /// A `Value::List` of integers, which is what a row of a list column arrives as.
    fn list(values: &[i32]) -> Value {
        Value::List {
            element: LogicalType::Integer,
            values: values.iter().map(|&v| Value::Integer(v)).collect(),
        }
    }

    fn list_column(rows: &[Value]) -> Vector {
        Vector::from_values(LogicalType::list(LogicalType::Integer), rows).unwrap()
    }

    #[test]
    fn a_list_column_is_one_child_and_a_range_per_row() {
        let rows = vec![list(&[1, 2, 3]), list(&[]), Value::Null, list(&[4])];
        let column = list_column(&rows);
        assert_eq!(column.form(), Form::List);
        assert_eq!(column.len(), 4);
        assert_eq!(column.logical_type(), &LogicalType::list(LogicalType::Integer));
        // Four rows and four elements, because a null and an empty list both contribute none.
        let (entries, child) = column.list_parts().expect("a list");
        assert_eq!(entries, [(0, 3), (3, 0), (3, 0), (3, 1)]);
        assert_eq!(child.len(), 4);
        assert_eq!(column.iter().collect::<Vec<_>>(), rows);
    }

    /// The one thing the entries cannot say on their own, so it has to be checked that the mask says
    /// it. An empty list is a row that is there and holds nothing, a null is a row that is not there,
    /// and both of them have an entry of length zero.
    #[test]
    fn an_empty_list_and_a_null_list_have_the_same_entry_and_are_different_rows() {
        let column = list_column(&[list(&[]), Value::Null]);
        let (entries, _) = column.list_parts().expect("a list");
        assert_eq!(entries[0].1, entries[1].1, "both entries are empty");
        assert!(!column.is_null_at(0), "an empty list is not null");
        assert!(column.is_null_at(1), "a null list is null");
        assert_eq!(column.value_at(0), list(&[]));
        assert_eq!(column.value_at(1), Value::Null);
    }

    #[test]
    fn slicing_a_list_column_shares_the_child_rather_than_copying_it() {
        let rows: Vec<Value> = (0..64).map(|row| list(&[row, row + 1, row + 2])).collect();
        let column = list_column(&rows);
        let cut = column.slice(8, 4).unwrap();
        assert_eq!(cut.form(), Form::List);
        assert_eq!(cut.iter().collect::<Vec<_>>(), rows[8..12]);
        // The entries are absolute positions in a child that was not cut, which is what makes the
        // cut eight bytes a row however long the lists are. The elements outside the range are still
        // there and nothing points at them.
        let (entries, child) = cut.list_parts().expect("a list");
        assert_eq!(entries[0], (24, 3));
        assert_eq!(child.len(), 192);
    }

    #[test]
    fn gathering_a_list_column_permutes_the_entries_and_leaves_the_child_alone() {
        let rows = vec![list(&[1]), list(&[2, 2]), list(&[3, 3, 3])];
        let column = list_column(&rows);
        let picked = column.gather(&[2, 0, 2]).unwrap();
        assert_eq!(
            picked.iter().collect::<Vec<_>>(),
            [list(&[3, 3, 3]), list(&[1]), list(&[3, 3, 3])]
        );
        // Two of the three rows are the same row, which is the case a run of offsets cannot write
        // down and a start and a length can. That is the whole reason this form carries both.
        assert_eq!(picked.list_parts().expect("a list").1.len(), 6);
    }

    #[test]
    fn a_gather_past_the_end_of_a_list_column_is_null_rather_than_somebody_elses_elements() {
        let column = list_column(&[list(&[1, 2]), list(&[3])]);
        let picked = column.gather(&[1, 9]).unwrap();
        assert_eq!(picked.value_at(0), list(&[3]));
        assert_eq!(picked.value_at(1), Value::Null);
    }

    #[test]
    fn a_list_of_lists_nests_as_far_as_it_is_written() {
        let outer = Value::List {
            element: LogicalType::list(LogicalType::Integer),
            values: vec![list(&[1, 2]), list(&[3])],
        };
        let column = Vector::from_values(
            LogicalType::list(LogicalType::list(LogicalType::Integer)),
            std::slice::from_ref(&outer),
        )
        .unwrap();
        assert_eq!(column.value_at(0), outer);
        assert_eq!(column.list_parts().expect("a list").1.form(), Form::List);
    }

    /// A list row is not bytes and not an integer, and a caller that asks for either gets nothing
    /// rather than the first element or a length. Both of those would be a wrong answer that a
    /// group by or a hash would read without complaining.
    #[test]
    fn the_scalar_readers_decline_a_list_instead_of_answering_about_its_elements() {
        let column = list_column(&[list(&[7])]);
        assert_eq!(column.signed_at(0), None);
        assert_eq!(column.bytes_at(0), None);
        assert_eq!(column.data(), None);
    }

    fn pair(a: i32, b: &str) -> Value {
        Value::Struct(vec![
            ("a".to_string(), Value::Integer(a)),
            ("b".to_string(), Value::Varchar(b.to_string())),
        ])
    }

    fn pair_type() -> LogicalType {
        LogicalType::Struct(vec![
            Field::new("a", LogicalType::Integer),
            Field::new("b", LogicalType::Varchar),
        ])
    }

    fn pair_column(rows: &[Value]) -> Vector {
        Vector::from_values(pair_type(), rows).unwrap()
    }

    #[test]
    fn a_struct_column_is_one_child_per_field_as_long_as_the_column() {
        let rows = vec![pair(1, "x"), pair(2, "y"), pair(3, "z")];
        let column = pair_column(&rows);
        assert_eq!(column.form(), Form::Struct);
        assert_eq!(column.len(), 3);
        assert_eq!(column.logical_type(), &pair_type());
        // Two children rather than two entries and a child, and both of them as long as the column,
        // which is the whole difference between this form and the list one.
        let children = column.struct_parts().expect("a struct");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].len(), 3);
        assert_eq!(children[1].len(), 3);
        assert_eq!(children[0].logical_type(), &LogicalType::Integer);
        assert_eq!(children[1].logical_type(), &LogicalType::Varchar);
        assert_eq!(column.iter().collect::<Vec<_>>(), rows);
    }

    /// Picking one field out of a struct is picking one child, which is the reason this accessor is
    /// public. A projection of `s.a` hands back a vector that already exists, so it costs a pointer
    /// rather than a pass over the rows, and that is only true while the children are full length.
    #[test]
    fn one_field_of_a_struct_column_is_a_column_that_is_already_there() {
        let column = pair_column(&[pair(10, "x"), pair(20, "y")]);
        let field = &column.struct_parts().expect("a struct")[0];
        assert_eq!(field.iter().collect::<Vec<_>>(), [Value::Integer(10), Value::Integer(20)]);
        assert_eq!(field.signed_at(1), Some(20), "the field is a scalar column and reads like one");
    }

    /// A null struct is a bit in the mask at the top and nothing deeper, which is how every other type
    /// records a null and is what DuckDB does. The row reads as a single null rather than as a struct of
    /// nulls, and the fields underneath are still their own columns.
    #[test]
    fn a_null_struct_is_the_mask_at_the_top_and_not_a_struct_full_of_nulls() {
        let column = pair_column(&[pair(1, "x"), Value::Null]);
        assert!(!column.is_null_at(0));
        assert!(column.is_null_at(1));
        assert_eq!(column.value_at(1), Value::Null);
        // A struct row whose every field happens to be null is a different row, and it is not null.
        let all_null = pair_column(&[Value::Struct(vec![
            ("a".to_string(), Value::Null),
            ("b".to_string(), Value::Null),
        ])]);
        assert!(!all_null.is_null_at(0), "a struct of nulls is a row that is there");
        assert_ne!(all_null.value_at(0), Value::Null);
    }

    #[test]
    fn slicing_a_struct_column_cuts_every_field_at_the_same_place() {
        let rows: Vec<Value> = (0..64).map(|row| pair(row, "s")).collect();
        let column = pair_column(&rows);
        let cut = column.slice(8, 4).unwrap();
        assert_eq!(cut.form(), Form::Struct);
        assert_eq!(cut.iter().collect::<Vec<_>>(), rows[8..12]);
        // The cut a list column does not have to do. A list shares its child untouched because the
        // entries carry the range, and a struct has no entry standing between the row and the child,
        // so every child is four rows long here rather than sixty four.
        for child in cut.struct_parts().expect("a struct") {
            assert_eq!(child.len(), 4);
        }
    }

    #[test]
    fn gathering_a_struct_column_gathers_every_field_at_the_same_positions() {
        let column = pair_column(&[pair(1, "x"), pair(2, "y"), pair(3, "z")]);
        let picked = column.gather(&[2, 0, 2]).unwrap();
        assert_eq!(picked.iter().collect::<Vec<_>>(), [pair(3, "z"), pair(1, "x"), pair(3, "z")]);
        for child in picked.struct_parts().expect("a struct") {
            assert_eq!(child.len(), 3, "a field is as long as the gather, not as the source");
        }
    }

    #[test]
    fn a_gather_past_the_end_of_a_struct_column_is_null_in_every_field_and_at_the_top() {
        let column = pair_column(&[pair(1, "x"), pair(2, "y")]);
        let picked = column.gather(&[1, 9]).unwrap();
        assert_eq!(picked.value_at(0), pair(2, "y"));
        assert_eq!(picked.value_at(1), Value::Null);
        for child in picked.struct_parts().expect("a struct") {
            assert!(child.is_null_at(1), "a row that came from nowhere has no field value either");
        }
    }

    /// The names are matched and not counted, because a caller holding a struct value built in a
    /// different order from the type's would otherwise get its columns transposed, and that is a wrong
    /// answer that reads as a right one.
    #[test]
    fn the_fields_of_a_struct_value_go_in_by_name_rather_than_by_position() {
        let swapped = Value::Struct(vec![
            ("b".to_string(), Value::Varchar("x".to_string())),
            ("a".to_string(), Value::Integer(1)),
        ]);
        let column = pair_column(&[swapped]);
        assert_eq!(column.value_at(0), pair(1, "x"));
        let wrong = Value::Struct(vec![
            ("a".to_string(), Value::Integer(1)),
            ("c".to_string(), Value::Varchar("x".to_string())),
        ]);
        let failed = Vector::from_values(pair_type(), &[wrong]);
        assert!(failed.is_err(), "a row with no b field is an error rather than a null b");
    }

    #[test]
    fn a_struct_built_from_children_takes_its_field_names_from_the_caller() {
        let column = Vector::structure(vec![
            ("a".to_string(), integers(&[1, 2, 3])),
            ("b".to_string(), integers(&[4, 5, 6])),
        ])
        .expect("two columns of three");
        assert_eq!(column.len(), 3);
        assert_eq!(
            column.logical_type(),
            &LogicalType::Struct(vec![
                Field::new("a", LogicalType::Integer),
                Field::new("b", LogicalType::Integer),
            ])
        );
        assert_eq!(
            column.value_at(1),
            Value::Struct(vec![
                ("a".to_string(), Value::Integer(2)),
                ("b".to_string(), Value::Integer(5)),
            ])
        );
    }

    /// The two mistakes this constructor makes easy, both refused rather than stored. A short field is
    /// the one that matters: it would be a struct that reads past the end of one of its own children,
    /// which is the same mistake `Vector::list` checks for at the other end.
    #[test]
    fn a_struct_of_uneven_children_or_of_no_children_is_refused() {
        let uneven = Vector::structure(vec![
            ("a".to_string(), integers(&[1, 2, 3])),
            ("b".to_string(), integers(&[4, 5])),
        ]);
        assert!(uneven.is_err(), "a field shorter than the struct");
        assert!(Vector::structure(vec![]).is_err(), "no field to take a length from");
    }

    #[test]
    fn a_struct_of_lists_and_a_list_of_structs_both_nest() {
        let ty =
            LogicalType::Struct(vec![Field::new("a", LogicalType::list(LogicalType::Integer))]);
        let row = Value::Struct(vec![("a".to_string(), list(&[1, 2]))]);
        let column = Vector::from_values(ty, std::slice::from_ref(&row)).unwrap();
        assert_eq!(column.value_at(0), row);
        assert_eq!(column.struct_parts().expect("a struct")[0].form(), Form::List);

        let outer = Value::List { element: pair_type(), values: vec![pair(1, "x"), pair(2, "y")] };
        let lists =
            Vector::from_values(LogicalType::list(pair_type()), std::slice::from_ref(&outer))
                .unwrap();
        assert_eq!(lists.value_at(0), outer);
        assert_eq!(lists.list_parts().expect("a list").1.form(), Form::Struct);
    }

    fn tags(pairs: &[(&str, &str)]) -> Value {
        Value::map(
            LogicalType::Varchar,
            LogicalType::Varchar,
            pairs
                .iter()
                .map(|&(key, value)| {
                    (Value::Varchar(key.to_string()), Value::Varchar(value.to_string()))
                })
                .collect(),
        )
    }

    fn tag_column(rows: &[Value]) -> Vector {
        Vector::from_values(LogicalType::map(LogicalType::Varchar, LogicalType::Varchar), rows)
            .unwrap()
    }

    /// A map is a list of two field structs, which is the whole design, so the test that says so is
    /// the one that reaches through both layers and finds the pieces where each of them puts them.
    #[test]
    fn a_map_column_is_a_list_whose_child_is_a_struct_of_keys_and_values() {
        let rows =
            vec![tags(&[("a", "b"), ("c", "d")]), tags(&[]), Value::Null, tags(&[("e", "f")])];
        let column = tag_column(&rows);
        assert_eq!(column.len(), 4);
        assert_eq!(
            column.logical_type(),
            &LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)
        );
        // The physical form is a list's, because the bytes are a list's. The logical type is what
        // remembers it is a map, which is the same split `LogicalType::physical` already makes.
        assert_eq!(column.form(), Form::List);
        let (entries, child) = column.list_parts().expect("the layout of a list");
        assert_eq!(entries, [(0, 2), (2, 0), (2, 0), (2, 1)]);
        assert_eq!(child.form(), Form::Struct);
        assert_eq!(
            child.logical_type(),
            &LogicalType::Struct(vec![
                Field::new(MAP_KEY, LogicalType::Varchar),
                Field::new(MAP_VALUE, LogicalType::Varchar),
            ])
        );
        // And the accessor that reaches through it hands back the two columns rather than the struct.
        let (entries, keys, values) = column.map_parts().expect("a map");
        assert_eq!(entries.len(), 4);
        assert_eq!(keys.text_at(0), Some("a"));
        assert_eq!(values.text_at(0), Some("b"));
        assert_eq!(column.iter().collect::<Vec<_>>(), rows);
    }

    /// The same distinction a list has, checked again here rather than assumed from the composition,
    /// because the empty map is the one every catalog table in D2 is full of and a null map is what a
    /// column with no tags at all would be.
    #[test]
    fn an_empty_map_and_a_null_map_are_different_rows() {
        let column = tag_column(&[tags(&[]), Value::Null]);
        assert!(!column.is_null_at(0), "an empty map is a row that is there");
        assert!(column.is_null_at(1));
        assert_eq!(column.value_at(0), tags(&[]));
        assert_eq!(column.value_at(1), Value::Null);
        assert_eq!(column.value_at(0).to_string(), "{}");
        assert_eq!(column.value_at(1).to_string(), "NULL");
    }

    /// A map prints `{a=b}` and a struct prints `{'a': b}`, both measured off the pin. They share a
    /// layout and they cannot share a printer, which is the one thing about this composition that does
    /// not fall out of it.
    #[test]
    fn a_map_prints_with_equals_signs_and_a_struct_prints_with_quoted_names() {
        assert_eq!(tags(&[("a", "b"), ("c", "d")]).to_string(), "{a=b, c=d}");
        assert_eq!(pair(1, "x").to_string(), "{'a': 1, 'b': x}");
        let numbers = Value::map(
            LogicalType::Integer,
            LogicalType::Integer,
            vec![(Value::Integer(1), Value::Integer(3)), (Value::Integer(2), Value::Integer(4))],
        );
        assert_eq!(numbers.to_string(), "{1=3, 2=4}");
        let null_value = Value::map(
            LogicalType::Varchar,
            LogicalType::Varchar,
            vec![(Value::Varchar("x".to_string()), Value::Null)],
        );
        assert_eq!(null_value.to_string(), "{x=NULL}");
    }

    /// A map inherits the list's cut and the list's gather, which is the payoff for storing it as one.
    /// Neither of these is code written for maps and both of them are worth a test that says the
    /// inheritance works, since the type is rewritten on the way through and a form that came back as a
    /// list would still read.
    #[test]
    fn cutting_and_gathering_a_map_keeps_it_a_map() {
        let rows: Vec<Value> =
            (0..16).map(|row| tags(&[("k", if row % 2 == 0 { "e" } else { "o" })])).collect();
        let column = tag_column(&rows);

        let cut = column.slice(4, 3).unwrap();
        assert!(matches!(cut.logical_type(), LogicalType::Map(_, _)), "still a map after a cut");
        assert_eq!(cut.iter().collect::<Vec<_>>(), rows[4..7]);
        // The child was not cut, the same as for a list, which is what makes the cut eight bytes a row.
        assert_eq!(cut.map_parts().expect("a map").1.len(), 16);

        let picked = column.gather(&[3, 0, 3]).unwrap();
        assert!(matches!(picked.logical_type(), LogicalType::Map(_, _)));
        assert_eq!(
            picked.iter().collect::<Vec<_>>(),
            [rows[3].clone(), rows[0].clone(), rows[3].clone()]
        );
        let past = column.gather(&[0, 99]).unwrap();
        assert_eq!(past.value_at(1), Value::Null);
    }

    #[test]
    fn a_map_built_from_two_columns_pairs_them_by_position() {
        let keys = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".to_string()), Value::Varchar("c".to_string())],
        )
        .unwrap();
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("b".to_string()), Value::Varchar("d".to_string())],
        )
        .unwrap();
        let column = Vector::map(vec![(0, 2), (2, 0)], keys, values).expect("two rows");
        assert_eq!(column.len(), 2);
        assert_eq!(
            column.logical_type(),
            &LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)
        );
        assert_eq!(column.value_at(0), tags(&[("a", "b"), ("c", "d")]));
        assert_eq!(column.value_at(1), tags(&[]));
        // The entry check the list constructor does is the one a map gets, so an entry past the end of
        // the pair of columns is refused here too rather than read as somebody else's keys.
        let short =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("a".to_string())]).unwrap();
        let other =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("b".to_string())]).unwrap();
        assert!(Vector::map(vec![(0, 9)], short, other).is_err(), "an entry past the end");
    }

    /// `map_parts` is about the logical type and `list_parts` is about the layout, so a list has to
    /// decline the first and a map has to answer the second. Getting that backwards would let a kernel
    /// written for maps read a list of two field structs as if it were one.
    #[test]
    fn a_list_is_not_a_map_however_much_its_child_looks_like_one() {
        let pairs = Value::List { element: pair_type(), values: vec![pair(1, "x")] };
        let column =
            Vector::from_values(LogicalType::list(pair_type()), std::slice::from_ref(&pairs))
                .unwrap();
        assert!(column.map_parts().is_none(), "a list of structs is a list");
        assert!(column.list_parts().is_some());
        let map = tag_column(&[tags(&[("a", "b")])]);
        assert!(map.map_parts().is_some());
        assert!(map.list_parts().is_some(), "a map has a list's layout and says so");
    }

    /// A struct row is not bytes and not an integer, and it stays that way when it has exactly one
    /// integer field, which is the case where answering about the field would look reasonable and would
    /// be a hash keyed on the wrong thing.
    #[test]
    fn the_scalar_readers_decline_a_struct_of_one_integer_field() {
        let ty = LogicalType::Struct(vec![Field::new("a", LogicalType::Integer)]);
        let row = Value::Struct(vec![("a".to_string(), Value::Integer(7))]);
        let column = Vector::from_values(ty, &[row]).unwrap();
        assert_eq!(column.signed_at(0), None);
        assert_eq!(column.bytes_at(0), None);
        assert_eq!(column.data(), None);
    }

    #[test]
    fn a_clustered_column_becomes_runs_and_reads_back_the_same() {
        let mut values = Vec::new();
        for (value, times) in [(7, 400), (8, 300), (7, 324)] {
            values.extend(std::iter::repeat_n(value, times));
        }
        let flat = integers(&values);
        let runs = flat.run_encoded().unwrap();
        assert_eq!(runs.form(), Form::Rle);
        assert_eq!(runs.run_parts().expect("runs").0, [400, 700, 1024]);
        assert_eq!(runs.len(), flat.len());
        assert_eq!(runs.iter().collect::<Vec<_>>(), flat.iter().collect::<Vec<_>>());
        assert!(
            runs.footprint() * 10 < flat.footprint(),
            "three runs against a thousand rows: {} against {}",
            runs.footprint(),
            flat.footprint()
        );
    }

    /// The check is worth having in both directions. A form that is only ever bigger than what it
    /// replaced is a form that costs a pass over the column to decide not to use.
    #[test]
    fn a_column_that_does_not_repeat_is_left_flat() {
        let flat = integers(&(0..1024).collect::<Vec<i32>>());
        assert_eq!(flat.run_encoded().unwrap().form(), Form::Flat);
        // Two runs over four rows is exactly break even on a four byte column, and break even is
        // not a reason to change form.
        assert_eq!(integers(&[1, 1, 2, 2]).run_encoded().unwrap().form(), Form::Flat);
        assert_eq!(integers(&[1, 1, 1, 2, 2]).run_encoded().unwrap().form(), Form::Rle);
    }

    #[test]
    fn two_nulls_beside_each_other_are_one_run_and_a_null_between_two_equals_is_a_break() {
        let mut values = vec![Value::Integer(4), Value::Integer(4)];
        values.extend([Value::Null, Value::Null, Value::Null]);
        values.extend(std::iter::repeat_n(Value::Integer(4), 5));
        let flat = Vector::from_values(LogicalType::Integer, &values).unwrap();
        let runs = flat.run_encoded().unwrap();
        assert_eq!(runs.run_parts().expect("runs").0, [2, 5, 10]);
        assert_eq!(runs.iter().collect::<Vec<_>>(), values);
    }

    #[test]
    fn slicing_runs_keeps_them_runs_and_cuts_the_first_and_last_one_back() {
        let flat = integers(&[1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
        let runs = flat.run_encoded().unwrap();
        let piece = runs.slice(3, 6).unwrap();
        assert_eq!(piece.form(), Form::Rle, "the form is the whole point");
        assert_eq!(piece.run_parts().expect("runs").0, [1, 5, 6]);
        assert_eq!(
            piece.iter().collect::<Vec<_>>(),
            flat.slice(3, 6).unwrap().iter().collect::<Vec<_>>()
        );
        assert_eq!(runs.slice(0, 0).unwrap().len(), 0);
        assert_eq!(runs.slice(0, 12).unwrap().form(), Form::Rle);
    }

    #[test]
    fn gathering_out_of_runs_walks_to_the_values_the_way_it_walks_a_dictionary() {
        let mut values = vec![Value::Varchar("red".into()); 4];
        values.extend([Value::Null, Value::Null, Value::Null]);
        values.extend(vec![Value::Varchar("blue".into()); 4]);
        let runs =
            Vector::from_values(LogicalType::Varchar, &values).unwrap().run_encoded().unwrap();
        assert_eq!(runs.form(), Form::Rle);
        let picked = runs.gather(&[8, 0, 5, 2]).unwrap();
        assert_eq!(picked.form(), Form::Flat, "a gather copies, whatever it gathered from");
        assert_eq!(
            picked.iter().collect::<Vec<_>>(),
            [values[8].clone(), values[0].clone(), Value::Null, values[2].clone()]
        );
        assert_eq!(runs.text_at(1), Some("red"));
        assert_eq!(runs.text_at(5), None, "a null has no text");
        assert_eq!(runs.flatten().unwrap().iter().collect::<Vec<_>>(), values);
    }

    /// A run length vector over a run length vector turns one search per row into two, and there is
    /// nothing in the engine that builds one, so it is refused rather than composed.
    #[test]
    fn runs_of_runs_are_refused_and_runs_of_a_dictionary_are_not() {
        let inner = integers(&[1, 1, 1, 1, 2]).run_encoded().unwrap();
        assert_eq!(inner.form(), Form::Rle);
        let error = Vector::runs(vec![2, 8], inner).unwrap_err();
        assert!(error.to_string().contains("runs of runs"), "{error}");

        let words = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        let dictionary = Vector::dictionary(vec![1, 0], words).unwrap();
        let stacked = Vector::runs(vec![4, 9], dictionary).unwrap();
        assert_eq!(stacked.len(), 9);
        assert_eq!(stacked.value_at(3), Value::Varchar("blue".into()));
        assert_eq!(stacked.value_at(4), Value::Varchar("red".into()));
    }

    #[test]
    fn run_ends_have_to_increase_and_there_is_one_value_for_each_of_them() {
        let values = integers(&[1, 2]);
        assert!(Vector::runs(vec![4], values.clone()).is_err(), "two values and one run");
        assert!(Vector::runs(vec![4, 4], values.clone()).is_err(), "an end that repeats");
        assert!(Vector::runs(vec![4, 2], values.clone()).is_err(), "an end that goes backwards");
        assert!(Vector::runs(vec![0, 2], values.clone()).is_err(), "a first run holding no rows");
        assert_eq!(Vector::runs(vec![4, 9], values).unwrap().len(), 9);
    }

    #[test]
    fn a_form_that_is_already_compact_is_left_where_it_is() {
        let constant = Vector::constant(LogicalType::Integer, Value::Integer(1), 1000);
        assert_eq!(constant.run_encoded().unwrap().form(), Form::Constant);
        assert_eq!(Vector::sequence(0, 1, 1000).run_encoded().unwrap().form(), Form::Sequence);
    }

    /// What makes one accessor cover both forms. A dictionary hands back the codes it stores and a
    /// run length vector works the same numbers out, and a kernel writing `values[at[row]]` reads
    /// the same rows out of either.
    #[test]
    fn both_forms_that_point_somewhere_hand_back_a_position_per_row() {
        let words = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        let runs = Vector::runs(vec![3, 5], words.clone()).unwrap();
        let (at, values) = runs.positions().expect("runs point somewhere");
        assert_eq!(at.as_ref(), [0, 0, 0, 1, 1]);
        assert_eq!(values.value_at(at[3] as usize), runs.value_at(3));

        let dictionary = Vector::dictionary(vec![1, 0, 1], words).unwrap();
        let (at, values) = dictionary.positions().expect("a dictionary points somewhere");
        assert_eq!(at.as_ref(), [1, 0, 1]);
        assert_eq!(values.value_at(at[0] as usize), dictionary.value_at(0));

        assert!(integers(&[1, 2, 3]).positions().is_none(), "a flat vector points at itself");
        assert!(Vector::sequence(0, 1, 4).positions().is_none(), "a sequence stores nothing");
    }

    #[test]
    fn slicing_a_dictionary_keeps_it_a_dictionary_where_gathering_would_not() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        let vector = Vector::dictionary(vec![0, 1, 1, 0, 1], values).unwrap();

        let piece = vector.slice(1, 3).unwrap();
        assert_eq!(piece.form(), Form::Dictionary, "the form is the whole point");
        assert_eq!(piece.len(), 3);
        assert_eq!(
            piece.iter().collect::<Vec<_>>(),
            [
                Value::Varchar("blue".into()),
                Value::Varchar("blue".into()),
                Value::Varchar("red".into())
            ]
        );
        assert_eq!(vector.gather(&[1, 2, 3]).unwrap().form(), Form::Flat, "which a gather loses");
    }

    #[test]
    fn slicing_a_dictionary_shares_the_dictionary_rather_than_copying_it() {
        // The assertion is about the address and not about the values, because the values were
        // right when the dictionary was copied too. A page holds one dictionary and is cut into a
        // chunk of codes at a time, so copying the dictionary here is a copy of every string in it
        // per chunk, and on a read of a ClickBench partition it was ten percent of the cycles.
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        let vector = Vector::dictionary(vec![0, 1, 1, 0, 1], values).unwrap();
        let Body::Dictionary { values: whole, .. } = &vector.body else {
            panic!("a dictionary vector holds a dictionary");
        };

        let piece = vector.slice(1, 3).unwrap();
        let Body::Dictionary { codes, values: cut, .. } = &piece.body else {
            panic!("a slice of a dictionary is a dictionary");
        };
        assert!(Arc::ptr_eq(whole, cut), "the cut copied the dictionary");
        assert_eq!(codes.as_slice(), &[1, 1, 0], "the codes are the part that is cut");

        // And a cut of a cut shares it too, since that is what a scan does to a page it reads twice.
        let again = piece.slice(1, 2).unwrap();
        let Body::Dictionary { values: cut, .. } = &again.body else {
            panic!("a slice of a slice of a dictionary is a dictionary");
        };
        assert!(Arc::ptr_eq(whole, cut), "the second cut copied the dictionary");
        assert_eq!(
            again.iter().collect::<Vec<_>>(),
            [Value::Varchar("blue".into()), Value::Varchar("red".into())]
        );
    }

    /// A parent column read for a link join, and the copy per chunk that not paging it was.
    ///
    /// The path is the one a kernel takes. A link join emits [`Body::Gathered`] over the parent and
    /// reads nothing, and the kernel that first wants the values flattens it, which is where the
    /// arena is either taken by handle or copied out of. The arena was already behind an `Arc`
    /// before this and every flatten still copied every byte it reached, because the question
    /// [`Buffer::is_shared`] answers is about the store inside the `Arc` rather than the `Arc`. On
    /// TPC-H q12 that was fourteen hundred copies a query out of a column of five distinct values.
    #[test]
    fn flattening_a_gather_off_a_paged_parent_takes_the_arena_rather_than_copying_it() {
        let arena = Arc::new(Buffer::from_vec(b"1-URGENT2-HIGH".to_vec()));
        let views = vec![
            StringView::over(b"1-URGENT", 0),
            StringView::over(b"2-HIGH", 8),
            StringView::over(b"1-URGENT", 0),
        ];
        let built = Vector::string_views(LogicalType::Varchar, views, arena).unwrap();
        let owned = match &built.body {
            Body::Views { arena, .. } => arena.is_shared(),
            _ => panic!("string views are a views body"),
        };
        assert!(!owned, "concat builds an arena rather than reading one, so it starts owned");

        let bytes = |vector: &Vector| match &vector.body {
            Body::Views { arena, .. } => arena.as_slice().as_ptr() as usize,
            Body::Flat(Data::Varlen(column)) => column.arena().as_ptr() as usize,
            _ => panic!("a string vector holds string bytes"),
        };
        let gathered = |parent: &Vector| {
            Vector::gathered(Arc::new(parent.clone()), Arc::new(vec![1, 0])).unwrap()
        };

        // Built again rather than cloned, because a clone would be a second holder of the arena and
        // paging would decline it, which is the case the test below this one is about.
        let paged = Vector::string_views(
            LogicalType::Varchar,
            built.shared_views().unwrap().0.to_vec(),
            Arc::new(Buffer::from_vec(b"1-URGENT2-HIGH".to_vec())),
        )
        .unwrap()
        .into_pages();
        assert_eq!(
            bytes(&gathered(&paged).flatten().unwrap()),
            bytes(&paged),
            "a flatten off a page shares the arena"
        );
        assert_ne!(
            bytes(&gathered(&built).flatten().unwrap()),
            bytes(&built),
            "and off an owned arena it copies, which is what this changed"
        );
        assert_eq!(
            gathered(&paged).flatten().unwrap().iter().collect::<Vec<_>>(),
            [Value::Varchar("2-HIGH".into()), Value::Varchar("1-URGENT".into())]
        );
    }

    /// An arena somebody else is still holding is left as it was, because the only way to page it
    /// would be to copy it and a copy is the thing the caller asked not to pay for.
    #[test]
    fn paging_a_string_column_whose_arena_has_another_holder_leaves_it_alone() {
        let arena = Arc::new(Buffer::from_vec(b"red".to_vec()));
        let vector =
            Vector::string_views(LogicalType::Varchar, vec![StringView::over(b"red", 0)], arena)
                .unwrap();
        // The clone is the other holder: both vectors point at the one arena.
        let paged = vector.clone().into_pages();
        match &paged.body {
            Body::Views { arena, .. } => assert!(!arena.is_shared(), "it was not ours to move"),
            _ => panic!("string views are a views body"),
        }
        assert_eq!(paged.iter().collect::<Vec<_>>(), [Value::Varchar("red".into())]);
    }

    /// Once the codes are a page, a cut and a clone of a coded column point at the same codes, which
    /// is what a scan does to every page of a dictionary encoded Parquet column.
    #[test]
    fn a_paged_dictionary_shares_its_codes_with_its_cuts_and_clones() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        let vector = Vector::dictionary(vec![0, 1, 1, 0, 1], values).unwrap().into_pages();
        let codes = |vector: &Vector| match &vector.body {
            Body::Dictionary { codes, .. } => codes.as_slice().as_ptr() as usize,
            _ => panic!("a dictionary vector holds a dictionary"),
        };
        assert_eq!(codes(&vector.slice(1, 3).unwrap()), codes(&vector) + 4, "the cut copied");
        assert_eq!(codes(&vector.clone()), codes(&vector), "the clone copied");
        assert_eq!(
            vector.slice(1, 3).unwrap().iter().collect::<Vec<_>>(),
            [
                Value::Varchar("blue".into()),
                Value::Varchar("blue".into()),
                Value::Varchar("red".into())
            ]
        );
    }

    #[test]
    fn a_slice_carries_the_nulls_that_were_in_its_range_and_not_the_others() {
        let vector =
            integers(&[1, 2, 3, 4]).with_validity(Validity::from_run(&[false, true, false, true]));
        let piece = vector.slice(1, 2).unwrap();
        assert!(piece.validity().is_valid(0));
        assert!(!piece.validity().is_valid(1));
        assert_eq!(piece.value_at(1), Value::Null);
    }

    #[test]
    fn slicing_a_sequence_moves_its_start_rather_than_writing_the_values_out() {
        let vector = Vector::sequence(100, 5, 10);
        let piece = vector.slice(3, 4).unwrap();
        assert_eq!(piece.form(), Form::Sequence);
        assert_eq!(
            piece.iter().collect::<Vec<_>>(),
            [Value::BigInt(115), Value::BigInt(120), Value::BigInt(125), Value::BigInt(130)]
        );
    }

    #[test]
    fn slicing_a_constant_is_a_shorter_constant() {
        let vector = Vector::constant(LogicalType::Integer, Value::Integer(9), 8);
        let piece = vector.slice(2, 3).unwrap();
        assert_eq!(piece.form(), Form::Constant);
        assert_eq!(piece.len(), 3);
        assert_eq!(piece.value_at(2), Value::Integer(9));
    }

    #[test]
    fn slicing_the_whole_vector_hands_it_back_as_it_was() {
        let vector = integers(&[1, 2, 3]);
        assert_eq!(
            vector.slice(0, 3).unwrap().iter().collect::<Vec<_>>(),
            [Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
    }

    /// The short way through a gather, a flat run with no nulls, answers what the long way does,
    /// and a position past the end still takes the long way and comes back null.
    #[test]
    fn a_gather_off_a_flat_run_with_no_nulls_answers_what_the_general_copy_does() {
        let rows: Vec<i32> = (0..50).map(|row| row * 3 - 20).collect();
        let vector = integers(&rows);
        let positions: Vec<u32> = [49, 0, 7, 7, 31, 2].into_iter().collect();
        let gathered = vector.gather(&positions).unwrap();
        assert_eq!(gathered.form(), Form::Flat);
        assert_eq!(
            gathered.iter().collect::<Vec<_>>(),
            positions.iter().map(|&at| Value::Integer(rows[at as usize])).collect::<Vec<_>>()
        );
        let past = vector.gather(&[3, 50]).unwrap();
        assert_eq!(past.iter().collect::<Vec<_>>(), [Value::Integer(-11), Value::Null]);
    }

    #[test]
    fn cutting_a_flat_body_answers_what_gathering_the_same_rows_answers() {
        // The cut of a flat body used to be written as a gather over the positions in the range,
        // and it is now a run copied out, so the two have to keep saying the same thing. Every
        // start and every length, with nulls in the range and out of it, since the validity is the
        // half of this that changed shape.
        let rows: Vec<i32> = (0..70).collect();
        let valid: Vec<bool> = (0..70).map(|row| row % 7 != 0 && row % 11 != 3).collect();
        let vector = integers(&rows).with_validity(Validity::from_run(&valid));
        for at in 0..70usize {
            for len in 0..=(70 - at) {
                let cut = vector.slice(at, len).unwrap();
                let positions: Vec<u32> = (at..at + len).map(|row| row as u32).collect();
                let gathered = vector.gather(&positions).unwrap();
                assert_eq!(cut.len(), len, "rows {at} to {}", at + len);
                assert_eq!(
                    cut.iter().collect::<Vec<_>>(),
                    gathered.iter().collect::<Vec<_>>(),
                    "rows {at} to {}",
                    at + len
                );
            }
        }
    }

    /// The flat body used to be the one form of a vector whose cut cost an allocation and a copy,
    /// and it is not any more when its buffer is a run inside a page. Asserted on the address,
    /// because the values are the same either way and the address is the whole claim.
    #[test]
    fn cutting_a_flat_body_over_a_page_does_not_copy_it() {
        let page = Arc::new((0i64..64).collect::<Vec<_>>());
        let address = page.as_ptr() as usize;
        let data = Data::Int64(Buffer::from_arc(Arc::clone(&page)));
        let vector = Vector::flat(LogicalType::BigInt, data).unwrap();
        let cut = vector.slice(16, 8).unwrap();
        assert_eq!(cut.form(), Form::Flat);
        assert_eq!(cut.len(), 8);
        let Some(Data::Int64(run)) = cut.data() else {
            panic!("the layout changed under the test")
        };
        assert!(run.is_shared(), "the cut copied the run out of the page");
        assert_eq!(run.as_slice().as_ptr() as usize, address + 16 * 8);
        assert_eq!(run.as_slice(), &(16i64..24).collect::<Vec<_>>()[..]);
        assert_eq!(cut.value_at(0), Value::BigInt(16));
        // And the same cut of an owned run says the same thing, by copying it.
        let owned = Vector::flat(LogicalType::BigInt, Data::Int64((0i64..64).collect())).unwrap();
        let copied = owned.slice(16, 8).unwrap();
        let Some(Data::Int64(run)) = copied.data() else {
            panic!("the layout changed under the test")
        };
        assert!(!run.is_shared());
        assert_eq!(run.as_slice(), &(16i64..24).collect::<Vec<_>>()[..]);
    }

    /// `into_pages` is how a producer says its values will be handed out many times. A flat body is
    /// the form it changes, and after it a copy of the vector is a reference count bump.
    #[test]
    fn a_vector_over_pages_is_copied_and_cut_without_its_values_moving() {
        let vector = integers(&[1, 2, 3, 4, 5, 6, 7, 8]).into_pages();
        let address = |vector: &Vector| match vector.data() {
            Some(Data::Int32(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("the layout changed under the test"),
        };
        let stored = address(&vector);
        assert_eq!(address(&vector.clone()), stored, "a copy moved the values");
        assert_eq!(address(&vector.slice(2, 4).unwrap()), stored + 2 * 4, "a cut moved the values");
        assert_eq!(
            vector.slice(2, 4).unwrap().iter().collect::<Vec<_>>(),
            [Value::Integer(3), Value::Integer(4), Value::Integer(5), Value::Integer(6)]
        );
        // Twice is not two pages.
        assert_eq!(address(&vector.clone().into_pages()), stored);
    }

    /// A cut, a gather and a flatten of a string column over a page all move views and no bytes.
    ///
    /// This is the string half of the paging that `a_vector_over_pages_is_copied_and_cut_without_
    /// its_values_moving` checks for a fixed width column, and it is worth its own test because a
    /// string column is two allocations rather than one: the cut that matters is the payload
    /// staying where it is while the views move.
    #[test]
    fn a_string_column_over_a_page_is_cut_and_gathered_without_its_payload_moving() {
        let long = ["the first of the long strings", "the second one", "and a third long one here"];
        let mut built = StringColumn::with_capacity(long.len());
        for text in long {
            built.push(text);
        }
        let vector = Vector::flat(LogicalType::Varchar, Data::Varlen(built.into_page())).unwrap();
        let payload = |vector: &Vector| match vector.data() {
            Some(Data::Varlen(column)) => column.arena().as_ptr() as usize,
            _ => panic!("the layout changed under the test"),
        };
        let stored = payload(&vector);
        let cut = vector.slice(1, 2).unwrap();
        assert_eq!(payload(&cut), stored, "a cut moved the payload");
        assert_eq!(cut.text_at(0), Some(long[1]));
        assert_eq!(cut.text_at(1), Some(long[2]));
        let gathered = vector.gather(&[2, 0]).unwrap();
        assert_eq!(payload(&gathered), stored, "a gather moved the payload");
        assert_eq!(gathered.text_at(0), Some(long[2]));
        assert_eq!(gathered.text_at(1), Some(long[0]));
        // And the same column with its own arena still copies, because sharing an owned arena
        // means cloning every byte of it including the bytes nobody asked for.
        let mut owned = StringColumn::with_capacity(long.len());
        for text in long {
            owned.push(text);
        }
        let held = Vector::flat(LogicalType::Varchar, Data::Varlen(owned)).unwrap();
        let copied = held.slice(1, 2).unwrap();
        assert_ne!(payload(&copied), payload(&held), "an owned payload was shared");
        assert_eq!(copied.text_at(0), Some(long[1]));
    }

    /// A flatten gives up the form and not the sharing. The views form is already views over an
    /// arena, so flattening one over a page is the views and nothing else, and the flat column
    /// that comes out reads the same strings out of the same bytes.
    #[test]
    fn flattening_string_views_over_a_page_keeps_the_page() {
        let mut built = StringColumn::with_capacity(2);
        built.push("a string too long to sit inside a view");
        built.push("another string that is also too long");
        let (views, arena) = built.into_page().into_parts();
        let stored = arena.as_slice().as_ptr() as usize;
        let vector = Vector::string_views(LogicalType::Varchar, views, Arc::new(arena)).unwrap();
        assert_eq!(vector.form(), Form::StringView);
        let flat = vector.flatten().unwrap();
        assert_eq!(flat.form(), Form::Flat);
        let Some(Data::Varlen(column)) = flat.data() else {
            panic!("the layout changed under the test")
        };
        assert_eq!(column.arena().as_ptr() as usize, stored, "the flatten moved the payload");
        assert_eq!(flat.text_at(0), Some("a string too long to sit inside a view"));
        assert_eq!(flat.text_at(1), Some("another string that is also too long"));
    }

    /// Every form that is not flat already shares what is expensive, so this is a no op on them and
    /// in particular does not flatten anything. A form that came back flat would be a column that
    /// lost its encoding on the way into a table.
    #[test]
    fn putting_a_vector_on_pages_does_not_change_any_other_form() {
        let dictionary = Vector::dictionary(
            vec![0, 1, 0, 1],
            Vector::from_values(
                LogicalType::Varchar,
                &[Value::Varchar("a".into()), Value::Varchar("b".into())],
            )
            .unwrap(),
        )
        .unwrap();
        let cases = [
            Vector::constant(LogicalType::Integer, Value::Integer(9), 4),
            Vector::sequence(4, 0, 1),
            dictionary,
        ];
        for vector in cases {
            let form = vector.form();
            let paged = vector.clone().into_pages();
            assert_eq!(paged.form(), form, "{form:?} changed form");
            assert_eq!(paged.iter().collect::<Vec<_>>(), vector.iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn cutting_a_flat_string_column_answers_what_gathering_it_answers() {
        // The string layout is the one whose cut is still a loop, and it is also the one where a
        // row is a view into an arena rather than a slot, so it gets the same treatment separately.
        // Both inline and out of line strings, since they are copied by different paths.
        let rows: Vec<String> =
            (0..40).map(|row| "x".repeat(row % 30) + &row.to_string()).collect();
        let values: Vec<Value> = rows.iter().map(|row| Value::Varchar(row.clone())).collect();
        let vector = Vector::from_values(LogicalType::Varchar, &values).unwrap().flatten().unwrap();
        assert_eq!(vector.form(), Form::Flat, "the cut under test is the flat one");
        for at in 0..40usize {
            for len in 0..=(40 - at) {
                let cut = vector.slice(at, len).unwrap();
                let positions: Vec<u32> = (at..at + len).map(|row| row as u32).collect();
                let gathered = vector.gather(&positions).unwrap();
                assert_eq!(
                    cut.iter().collect::<Vec<_>>(),
                    gathered.iter().collect::<Vec<_>>(),
                    "rows {at} to {}",
                    at + len
                );
            }
        }
    }

    #[test]
    fn a_slice_past_the_end_is_an_error_rather_than_a_short_vector() {
        let error = integers(&[1, 2, 3]).slice(2, 2).unwrap_err();
        assert!(error.to_string().contains("of a vector of 3"), "{error}");
    }

    #[test]
    fn the_vector_size_is_the_one_the_design_is_built_around() {
        // 8192, which is four times DuckDB's 2048, measured in #480 against 1024, 2048, 4096 and
        // 32768. What the rest of the code assumes about it is not the value but the shape: a
        // multiple of 1024, which is the FastLanes unit and is what makes a validity mask a whole
        // number of u64 words with none of them half used.
        assert_eq!(VECTOR_SIZE, 8192);
        assert_eq!(VECTOR_SIZE % 1024, 0);
        assert_eq!(VECTOR_SIZE % 64, 0);
        assert_eq!(VECTOR_SIZE / 64, 128, "the words in a validity mask");
    }

    #[test]
    fn a_flat_vector_reads_back_what_was_put_in_it() {
        let vector = integers(&[1, 2, 3]);
        assert_eq!(vector.form(), Form::Flat);
        assert_eq!(vector.len(), 3);
        assert_eq!(vector.value_at(1), Value::Integer(2));
        assert_eq!(
            vector.iter().collect::<Vec<_>>(),
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
    }

    #[test]
    fn a_vector_built_from_values_reads_the_same_values_back() {
        let vector = Vector::from_values(
            LogicalType::Varchar,
            &[
                Value::Varchar("a".to_string()),
                Value::Null,
                Value::Varchar("a string too long to sit inside a view".to_string()),
            ],
        )
        .expect("strings and a null");
        assert_eq!(vector.len(), 3);
        assert_eq!(vector.value_at(0), Value::Varchar("a".to_string()));
        assert_eq!(vector.value_at(1), Value::Null);
        assert_eq!(
            vector.value_at(2),
            Value::Varchar("a string too long to sit inside a view".to_string())
        );
    }

    /// A null still occupies a position. If it did not then every value after it would read back
    /// one place to the left, which is the kind of bug that looks like a storage bug for a week.
    #[test]
    fn a_null_in_the_middle_does_not_move_the_values_after_it() {
        let vector = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        )
        .expect("integers and a null");
        assert_eq!(vector.value_at(2), Value::Integer(3));
        assert!(vector.validity().has_nulls(3), "the middle one is null");
    }

    #[test]
    fn a_value_the_type_cannot_hold_is_refused() {
        let wrong = Vector::from_values(LogicalType::Integer, &[Value::Varchar("x".to_string())]);
        assert!(wrong.is_err(), "a string is not an integer");
    }

    #[test]
    fn a_type_that_does_not_match_its_layout_is_refused_at_construction() {
        // One comparison here against a wrong answer read out three layers later.
        let wrong = Vector::flat(LogicalType::Varchar, Data::Int32(vec![1].into()));
        assert!(wrong.is_err());
        let right = Vector::flat(LogicalType::Date, Data::Int32(vec![1].into()));
        assert!(right.is_ok(), "a date is stored in an i32 and that has to be allowed");
    }

    #[test]
    fn a_constant_vector_costs_one_value_whatever_its_length() {
        let vector = Vector::constant(LogicalType::Integer, Value::Integer(7), VECTOR_SIZE);
        assert_eq!(vector.form(), Form::Constant);
        assert_eq!(vector.len(), VECTOR_SIZE);
        assert_eq!(vector.value_at(0), Value::Integer(7));
        assert_eq!(vector.value_at(VECTOR_SIZE - 1), Value::Integer(7));
        assert_eq!(vector.value_at(VECTOR_SIZE), Value::Null, "past the end is null, not a panic");
    }

    #[test]
    fn a_constant_null_is_all_invalid_without_being_told() {
        let vector = Vector::constant(LogicalType::Integer, Value::Null, 8);
        assert_eq!(vector.validity(), &Validity::AllInvalid);
        assert_eq!(vector.value_at(3), Value::Null);
    }

    #[test]
    fn a_sequence_vector_is_sixteen_bytes_of_row_identifiers() {
        let vector = Vector::sequence(100, 1, VECTOR_SIZE);
        assert_eq!(vector.form(), Form::Sequence);
        assert_eq!(vector.value_at(0), Value::BigInt(100));
        assert_eq!(vector.value_at(923), Value::BigInt(1023));
        let stepped = Vector::sequence(0, 5, 4);
        assert_eq!(
            stepped.iter().collect::<Vec<_>>(),
            vec![Value::BigInt(0), Value::BigInt(5), Value::BigInt(10), Value::BigInt(15)]
        );
    }

    #[test]
    fn a_dictionary_vector_reads_through_its_codes() {
        let mut column = StringColumn::new();
        column.push("red");
        column.push("green");
        let values = Vector::flat(LogicalType::Varchar, Data::Varlen(column)).unwrap();
        let vector = Vector::dictionary(vec![0, 1, 1, 0], values).unwrap();
        assert_eq!(vector.form(), Form::Dictionary);
        assert_eq!(vector.logical_type(), &LogicalType::Varchar);
        assert_eq!(vector.value_at(2), Value::Varchar("green".into()));
        assert_eq!(vector.len(), 4);
    }

    /// The accessor a group by keys a string column through, which has to agree with `value_at` on
    /// every position or two rows holding one string end up in two groups.
    #[test]
    fn text_is_read_where_it_already_is_for_the_forms_that_store_it() {
        let mut column = StringColumn::new();
        column.push("red");
        column.push("green");
        column.push("");
        let flat = Vector::flat(LogicalType::Varchar, Data::Varlen(column)).unwrap();
        for index in 0..flat.len() {
            assert_eq!(flat.text_at(index).map(str::to_string), text_of(&flat.value_at(index)));
        }
        let dictionary = Vector::dictionary(vec![1, 0, 1, 2], flat).unwrap();
        for index in 0..dictionary.len() {
            assert_eq!(
                dictionary.text_at(index).map(str::to_string),
                text_of(&dictionary.value_at(index))
            );
        }
        assert_eq!(dictionary.text_at(4), None, "past the end");
    }

    /// The forms and types that have no text to hand back, which a caller answers by falling back
    /// to `value_at`. A blob is the one that would be a correctness bug rather than a slow path,
    /// since its bytes are not required to be text and it is not a `VARCHAR` either way.
    #[test]
    fn text_is_refused_where_it_is_not_stored_as_itself() {
        let nulls =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("red".into()), Value::Null])
                .unwrap();
        assert_eq!(nulls.text_at(0), Some("red"));
        assert_eq!(nulls.text_at(1), None, "a null has no text");
        let constant = Vector::constant(LogicalType::Varchar, Value::Varchar("red".into()), 3);
        assert_eq!(constant.text_at(0), None, "a constant is not stored per position");
        assert_eq!(integers(&[1, 2]).text_at(0), None, "an integer is not text");
        let mut bytes = StringColumn::new();
        bytes.push("red");
        let blob = Vector::flat(LogicalType::Blob, Data::Varlen(bytes)).unwrap();
        assert_eq!(blob.text_at(0), None, "a blob is not a varchar");
    }

    /// The accessor a group by keys an integer column through, which has to agree with `value_at`
    /// on every position or two rows holding one number end up in two groups.
    #[test]
    fn a_signed_integer_is_read_where_it_already_is_for_the_forms_that_store_it() {
        let flat = integers(&[7, -3, 0, 2]);
        for index in 0..flat.len() {
            assert_eq!(flat.signed_at(index), signed_of(&flat.value_at(index)), "flat {index}");
        }
        let dictionary = Vector::dictionary(vec![1, 0, 3, 2], flat).unwrap();
        for index in 0..dictionary.len() {
            assert_eq!(
                dictionary.signed_at(index),
                signed_of(&dictionary.value_at(index)),
                "dictionary {index}"
            );
        }
        assert_eq!(dictionary.signed_at(4), None, "past the end");

        let runs = Vector::runs(vec![2, 5], integers(&[4, 9])).unwrap();
        for index in 0..runs.len() {
            assert_eq!(runs.signed_at(index), signed_of(&runs.value_at(index)), "run {index}");
        }
        let constant = Vector::constant(LogicalType::BigInt, Value::BigInt(11), 3);
        assert_eq!(constant.signed_at(2), Some(11));
        let sequence = Vector::sequence(100, 5, 4);
        for index in 0..sequence.len() {
            assert_eq!(
                sequence.signed_at(index),
                signed_of(&sequence.value_at(index)),
                "sequence {index}"
            );
        }
    }

    /// A window of a shared page packs exactly when the same rows owned would, and a range its
    /// type cannot hold at the width it needs stays flat rather than failing. A load of ClickBench
    /// `hits` hit both: its windows were judged by their share of the page, packed at 32 bits, and
    /// the packed form refused a range that ran past `i32::MAX`.
    #[test]
    fn a_window_of_a_page_packs_the_way_the_same_rows_owned_do() {
        let wide: Vec<i32> = (0..122_880)
            .map(|at| if at % 2 == 0 { i32::MIN + 5 + at } else { i32::MAX - 9 - at })
            .collect();
        let narrow: Vec<i32> = (0..122_880).map(|at| 1_000 + at % 200).collect();
        for values in [wide, narrow] {
            let page = integers(&values).into_pages();
            let window = page.slice(0, 8_192).unwrap();
            let owned = integers(&values[..8_192]);
            let packed_window = window.bit_packed().unwrap();
            let packed_owned = owned.bit_packed().unwrap();
            assert_eq!(
                packed_window.packed_parts().is_some(),
                packed_owned.packed_parts().is_some()
            );
            for at in [0, 1, 4_095, 8_191] {
                assert_eq!(packed_window.value_at(at), owned.value_at(at));
            }
        }
    }

    /// The forms and types that have no integer to hand back, which a caller answers by falling
    /// back to `value_at`.
    #[test]
    fn a_signed_integer_is_refused_where_it_is_not_stored_as_itself() {
        let nulls =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(4), Value::Null]).unwrap();
        assert_eq!(nulls.signed_at(0), Some(4));
        assert_eq!(nulls.signed_at(1), None, "a null is not a number");
        let packed = integers(&[1, 2, 3, 1]).bit_packed().unwrap();
        assert_eq!(packed.signed_at(0), Some(1), "a packed integer is read in code space");
        let mut bytes = StringColumn::new();
        bytes.push("red");
        let text = Vector::flat(LogicalType::Varchar, Data::Varlen(bytes)).unwrap();
        assert_eq!(text.signed_at(0), None, "a string is not a number");
        let double = Vector::flat(LogicalType::Double, Data::Float64(vec![1.5].into())).unwrap();
        assert_eq!(double.signed_at(0), None, "a double is not a signed integer");
    }

    /// The block form has to agree with the row at a time form on every position of every shape it
    /// answers for, because a caller picks one of the two and a group by that read two different
    /// numbers for one row would put that row in two groups.
    #[test]
    fn a_block_of_signed_integers_holds_what_the_row_at_a_time_accessor_hands_back() {
        let mut out = Vec::new();
        let shapes = [
            integers(&[7, -3, 0, 2]),
            Vector::flat(LogicalType::Integer, Data::Int32(vec![5, -6, 7].into())).unwrap(),
            Vector::flat(LogicalType::SmallInt, Data::Int16(vec![1, -2].into())).unwrap(),
            Vector::flat(LogicalType::TinyInt, Data::Int8(vec![-128, 127].into())).unwrap(),
            Vector::constant(LogicalType::BigInt, Value::BigInt(11), 3),
            Vector::sequence(100, 5, 4),
            integers(&[1, 2, 3, 1]).bit_packed().unwrap(),
            Vector::dictionary(vec![1, 0, 1, 3], integers(&[7, -3, 0, 2])).unwrap(),
            Vector::dictionary(
                vec![2, 2, 0],
                Vector::flat(LogicalType::SmallInt, Data::Int16(vec![9, -9, 4].into())).unwrap(),
            )
            .unwrap(),
        ];
        for column in &shapes {
            assert!(column.signed_block(&mut out), "{:?} hands over a block", column.form());
            assert_eq!(out.len(), column.len(), "{:?} filled the whole chunk", column.form());
            for (index, &held) in out.iter().enumerate() {
                assert_eq!(
                    Some(i128::from(held)),
                    column.signed_at(index),
                    "{:?} at {index}",
                    column.form()
                );
            }
        }
    }

    /// The gathered form reads what the row at a time accessor reads at the rows it is given, and
    /// refuses a row past the end and a vector that is not flat, leaving nothing behind.
    #[test]
    fn a_gather_of_signed_integers_holds_what_the_row_at_a_time_accessor_hands_back() {
        let mut out = Vec::new();
        let at = [0, 2, 2, 3];
        let shapes = [
            integers(&[7, -3, 0, 2]),
            Vector::flat(LogicalType::Integer, Data::Int32(vec![5, -6, 7, -8].into())).unwrap(),
            Vector::flat(LogicalType::TinyInt, Data::Int8(vec![-128, 127, 1, 0].into())).unwrap(),
        ];
        for column in &shapes {
            assert!(column.signed_gather(&at, &mut out), "{:?} is gathered", column.logical_type());
            let wanted: Vec<i64> = at
                .iter()
                .map(|&row| i64::try_from(column.signed_at(row as usize).unwrap()).unwrap())
                .collect();
            assert_eq!(out, wanted);
        }
        let short = integers(&[1, 2, 3]);
        assert!(!short.signed_gather(&at, &mut out), "row 3 is past the end");
        assert!(out.is_empty());
        assert!(!Vector::sequence(100, 5, 4).signed_gather(&at, &mut out));
        assert!(integers(&[1]).signed_gather(&[], &mut out) && out.is_empty());
    }

    /// What the block form will not answer for, where the caller reads the vector a row at a time
    /// instead. A null is not one of them: it writes whatever sits under it and the caller reads the
    /// null from the column.
    #[test]
    fn a_block_is_refused_for_the_shapes_it_would_have_to_gather_or_widen() {
        let mut out = Vec::new();
        let nulled =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(4), Value::Null]).unwrap();
        assert!(
            !Vector::dictionary(vec![1, 0], nulled).unwrap().signed_block(&mut out),
            "a dictionary with a null entry would hand its row over as a number"
        );
        assert!(!Vector::runs(vec![2, 5], integers(&[4, 9])).unwrap().signed_block(&mut out));
        let wide = Vector::flat(LogicalType::HugeInt, Data::Int128(vec![1, 2].into())).unwrap();
        assert!(!wide.signed_block(&mut out), "a hugeint does not fit sixty four bits");
        let double = Vector::flat(LogicalType::Double, Data::Float64(vec![1.5].into())).unwrap();
        assert!(!double.signed_block(&mut out), "a double is not a signed integer");
        assert!(out.is_empty(), "a refusal leaves the buffer empty");

        let nulls =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(4), Value::Null]).unwrap();
        assert!(nulls.signed_block(&mut out), "a flat column with nulls still hands over");
        assert_eq!(out[0], 4);
    }

    /// Asked once for a chunk, and it has to agree with `is_null_at` asked for every row of it.
    #[test]
    fn a_vector_says_whether_it_holds_any_null_at_all() {
        let flat = integers(&[7, -3, 0, 2]);
        assert!(flat.none_null());
        let nulls =
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(4), Value::Null]).unwrap();
        assert!(!nulls.none_null());
        assert!(Vector::dictionary(vec![1, 0], flat.clone()).unwrap().none_null());
        // The null is in the dictionary rather than in the mask, which is the case the row at a time
        // form reads through for and the reason this one does too.
        let holed = Vector::dictionary(vec![0, 0], nulls.clone()).unwrap();
        assert!(!holed.none_null(), "a dictionary is read through to its values");
        assert!(!holed.is_null_at(0), "and no code points at the null it holds");
        assert!(Vector::runs(vec![2, 5], integers(&[4, 9])).unwrap().none_null());
        assert!(!Vector::runs(vec![1, 2], nulls).unwrap().none_null());
        assert!(Vector::constant(LogicalType::BigInt, Value::BigInt(11), 3).none_null());
        assert!(!Vector::constant(LogicalType::BigInt, Value::Null, 3).none_null());
    }

    /// The integer of a value, for comparing `signed_at` against `value_at` position by position.
    fn signed_of(value: &Value) -> Option<i128> {
        match value {
            Value::TinyInt(x) => Some(i128::from(*x)),
            Value::SmallInt(x) => Some(i128::from(*x)),
            Value::Integer(x) | Value::Date(x) => Some(i128::from(*x)),
            Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => Some(i128::from(*x)),
            Value::HugeInt(x) | Value::Decimal { unscaled: x, .. } => Some(*x),
            _ => None,
        }
    }

    /// The text of a value, for comparing `text_at` against `value_at` position by position.
    fn text_of(value: &Value) -> Option<String> {
        match value {
            Value::Varchar(text) => Some(text.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_dictionary_code_past_the_end_is_refused() {
        // The alternative is a silent read of the wrong value, which is the failure mode the
        // entire M3 design has to be careful about.
        let values = integers(&[1, 2]);
        assert!(Vector::dictionary(vec![0, 2], values).is_err());
        // The check runs on the highest code rather than the first bad one, so it has to say that
        // no codes at all is fine even when there are no values for them to point at either.
        let empty = Vector::dictionary(Vec::new(), integers(&[])).expect("no codes, no values");
        assert_eq!(empty.len(), 0);
        // And a code of zero against an empty dictionary is still past the end.
        assert!(Vector::dictionary(vec![0], integers(&[])).is_err());
    }

    #[test]
    fn every_form_flattens_to_the_same_values_it_reads_out() {
        // This is the shape of the equivalence testing in spec/16-testing.md section 16.2, in
        // miniature and long before there is an encoded kernel to point it at. A form that reads
        // out one way and flattens another is the exact bug that testing exists to catch.
        let mut column = StringColumn::new();
        column.push("alpha");
        column.push("beta");
        let dictionary = Vector::dictionary(
            vec![1, 0, 1],
            Vector::flat(LogicalType::Varchar, Data::Varlen(column)).unwrap(),
        )
        .unwrap();
        let cases = [
            Vector::constant(LogicalType::Integer, Value::Integer(3), 5),
            Vector::sequence(7, -2, 5),
            dictionary,
        ];
        for vector in cases {
            let flat = vector.flatten().unwrap();
            assert_eq!(flat.form(), Form::Flat);
            assert_eq!(flat.len(), vector.len());
            for index in 0..vector.len() {
                assert_eq!(flat.value_at(index), vector.value_at(index), "at {index}");
            }
        }
    }

    #[test]
    fn a_null_still_occupies_a_position_after_flattening() {
        // The reason push_value writes a zero for a null rather than skipping it. A run of data
        // with a hole in it puts every value after the hole in the wrong place, and the validity
        // mask is what says the position is null.
        let vector = Vector::sequence(0, 1, 4).with_validity(Validity::from_iter(4, |i| i != 1));
        let flat = vector.flatten().unwrap();
        assert_eq!(flat.value_at(0), Value::BigInt(0));
        assert_eq!(flat.value_at(1), Value::Null);
        assert_eq!(flat.value_at(2), Value::BigInt(2));
        assert_eq!(flat.value_at(3), Value::BigInt(3));
    }

    /// A dictionary holds its nulls in the vector it points at, so its own validity is all valid
    /// and reading that instead of the values turns a null into whatever zero means for the type.
    /// A filter over a nullable column produces exactly this vector, so the bug reaches a result
    /// set as `LEFT JOIN` padding that comes back as zeros.
    #[test]
    fn a_null_behind_a_dictionary_survives_flattening() {
        let values =
            Vector::from_values(LogicalType::Integer, &[Value::Integer(3), Value::Null]).unwrap();
        let dictionary = Vector::dictionary(vec![1, 0, 1], values).unwrap();
        let flat = dictionary.flatten().unwrap();
        assert_eq!(flat.value_at(0), Value::Null);
        assert_eq!(flat.value_at(1), Value::Integer(3));
        assert_eq!(flat.value_at(2), Value::Null);
    }

    /// The property that makes `gather` usable at all: it has to be the same function as reading the
    /// wanted positions one at a time, over every form, or compaction changes answers.
    #[test]
    fn gathering_reads_what_reading_one_position_at_a_time_reads() {
        let mut column = StringColumn::new();
        column.push("alpha");
        column.push("beta");
        column.push("gamma");
        let cases = [
            integers(&[10, 20, 30, 40]),
            integers(&[10, 20, 30, 40]).with_validity(Validity::from_iter(4, |i| i != 2)),
            Vector::constant(LogicalType::Integer, Value::Integer(9), 4),
            Vector::sequence(100, -7, 4),
            Vector::sequence(100, -7, 4).with_validity(Validity::from_iter(4, |i| i % 2 == 0)),
            Vector::dictionary(
                vec![2, 0, 1, 2],
                Vector::flat(LogicalType::Varchar, Data::Varlen(column)).unwrap(),
            )
            .unwrap(),
            Vector::dictionary(
                vec![1, 0, 1, 0],
                Vector::from_values(LogicalType::Integer, &[Value::Integer(5), Value::Null])
                    .unwrap(),
            )
            .unwrap(),
        ];
        let wanted = [3_u32, 0, 2, 2, 1];
        for vector in cases {
            let gathered = vector.gather(&wanted).unwrap();
            assert_eq!(gathered.len(), wanted.len());
            assert_eq!(gathered.logical_type(), vector.logical_type());
            for (slot, &index) in wanted.iter().enumerate() {
                assert_eq!(
                    gathered.value_at(slot),
                    vector.value_at(index as usize),
                    "slot {slot} of {:?}",
                    vector.form()
                );
            }
        }
    }

    /// A gather past the end is not an error, because the selection that produced the indices is
    /// checked by its caller and the one thing that must not happen here is a read of the wrong
    /// value. An index nothing answers is null, which is what an outer join pad needs anyway.
    #[test]
    fn gathering_a_position_that_is_not_there_is_a_null_and_not_a_wrong_value() {
        let vector = integers(&[1, 2, 3]);
        let gathered = vector.gather(&[2, 9]).unwrap();
        assert_eq!(gathered.value_at(0), Value::Integer(3));
        assert_eq!(gathered.value_at(1), Value::Null);
    }

    /// The vector with nothing in it at all, which is what an untyped `NULL` is stored as. Every
    /// position asked for is past its end, so the answer is nulls and the length has to be the
    /// length that was asked for rather than the length that was there.
    #[test]
    fn gathering_from_a_vector_of_no_values_is_that_many_nulls() {
        let vector = Vector::flat(LogicalType::Null, Data::Empty).unwrap();
        let gathered = vector.gather(&[0, 1, 2]).unwrap();
        assert_eq!(gathered.len(), 3);
        assert_eq!(gathered.value_at(0), Value::Null);
        assert_eq!(gathered.value_at(2), Value::Null);
    }

    /// Every position holds the same value, so a gather with no hole in it has nothing to copy and
    /// the result is the constant again rather than a run of a thousand copies of it.
    #[test]
    fn gathering_a_constant_stays_a_constant() {
        let vector = Vector::constant(LogicalType::Integer, Value::Integer(4), 100);
        let gathered = vector.gather(&[7, 7, 99]).unwrap();
        assert_eq!(gathered.form(), Form::Constant);
        assert_eq!(gathered.len(), 3);
        assert_eq!(gathered.value_at(2), Value::Integer(4));
    }

    /// A dictionary over a dictionary is what a second filter over an already filtered chunk builds,
    /// and the gather has to walk to the bottom of that chain rather than one step down it. The
    /// constructor composes the ordinary chain away, so the one built here is the kind it cannot,
    /// which is a level holding nulls of its own.
    #[test]
    fn gathering_walks_a_dictionary_over_a_dictionary_to_the_values() {
        let inner = Vector::dictionary(vec![2, 1, 0], integers(&[7, 8, 9]))
            .unwrap()
            .with_validity(Validity::from_iter(3, |index| index != 2));
        let outer = Vector::dictionary(vec![1, 2], inner).unwrap();
        let gathered = outer.gather(&[0, 1]).unwrap();
        assert_eq!(gathered.form(), Form::Flat);
        assert_eq!(gathered.value_at(0), Value::Integer(8));
        assert_eq!(gathered.value_at(1), Value::Null);
    }

    /// Two filters over one chunk build a dictionary over a dictionary, four conjuncts pushed down
    /// separately build four levels of it, and every level is a dependent load on every later read
    /// of every row plus a code array that cannot be freed. Composing at construction is one pass
    /// over the codes the range check was walking anyway.
    #[test]
    fn a_dictionary_over_a_dictionary_is_composed_into_one_level() {
        let inner = Vector::dictionary(vec![2, 1, 0], integers(&[7, 8, 9])).unwrap();
        let outer = Vector::dictionary(vec![1, 2], inner).unwrap();
        let (codes, values) = outer.dictionary_parts().unwrap();
        assert_eq!(codes, [1, 0]);
        assert_eq!(values.form(), Form::Flat);
        assert_eq!(outer.value_at(0), Value::Integer(8));
        assert_eq!(outer.value_at(1), Value::Integer(7));
    }

    /// The invariant stated as the thing it is there for, which is that the depth does not grow with
    /// the number of filters. Four levels stacked one at a time are one level at the end of it.
    #[test]
    fn stacking_dictionaries_does_not_make_them_deeper() {
        let mut vector = integers(&[10, 20, 30, 40]);
        for _ in 0..4 {
            vector = Vector::dictionary(vec![3, 2, 1, 0], vector).unwrap();
        }
        let (codes, values) = vector.dictionary_parts().unwrap();
        assert_eq!(values.form(), Form::Flat);
        assert_eq!(codes, [0, 1, 2, 3]);
        assert_eq!(
            vector.iter().collect::<Vec<_>>(),
            integers(&[10, 20, 30, 40]).iter().collect::<Vec<_>>()
        );
    }

    /// Composing has to carry the nulls down with it. The values hold them, the codes point at them,
    /// and a composed code that lands on a null position is still a null.
    #[test]
    fn composing_a_dictionary_keeps_the_nulls_its_values_hold() {
        let values =
            Vector::from_values(LogicalType::Integer, &[Value::Integer(3), Value::Null]).unwrap();
        let inner = Vector::dictionary(vec![1, 0, 1], values).unwrap();
        let outer = Vector::dictionary(vec![0, 1], inner).unwrap();
        assert_eq!(outer.dictionary_parts().unwrap().1.form(), Form::Flat);
        assert_eq!(outer.value_at(0), Value::Null);
        assert_eq!(outer.value_at(1), Value::Integer(3));
    }

    /// The one level composition cannot go past. A dictionary that was given a validity of its own is
    /// saying its nulls are at that level rather than in the values, and pointing the outer codes
    /// straight at the values would read through the holes instead of stopping at them.
    #[test]
    fn a_dictionary_holding_its_own_nulls_is_not_composed_past() {
        let inner = Vector::dictionary(vec![0, 1, 2], integers(&[1, 2, 3]))
            .unwrap()
            .with_validity(Validity::from_iter(3, |index| index != 1));
        let outer = Vector::dictionary(vec![1, 2, 0], inner).unwrap();
        assert_eq!(outer.dictionary_parts().unwrap().1.form(), Form::Dictionary);
        assert_eq!(outer.value_at(0), Value::Null);
        assert_eq!(outer.value_at(1), Value::Integer(3));
        assert_eq!(outer.value_at(2), Value::Integer(1));
    }

    /// The difference between the two questions about nulls, which a group by got wrong. A filtered
    /// chunk is dictionary vectors, those are built with every row marked present at their own
    /// level, and the nulls are down in the values. So the mask says the row has a value and the
    /// row does not.
    #[test]
    fn a_null_behind_a_dictionary_reads_as_null_even_though_the_mask_says_otherwise() {
        let values = Vector::flat(LogicalType::Integer, Data::Int32(vec![0, 7].into()))
            .unwrap()
            .with_validity(Validity::from_iter(2, |index| index != 0));
        let vector = Vector::dictionary(vec![0, 1, 0], values).unwrap();
        assert!(vector.validity().is_valid(0), "the mask at this level says present");
        assert!(vector.is_null_at(0));
        assert!(!vector.is_null_at(1));
        assert!(vector.is_null_at(2));
        assert!(vector.is_null_at(3), "a row past the end is null");
    }

    /// The same for runs, which are built the same way and keep their nulls in the same place.
    #[test]
    fn a_null_inside_a_run_reads_as_null_even_though_the_mask_says_otherwise() {
        let values = Vector::flat(LogicalType::Integer, Data::Int32(vec![0, 7].into()))
            .unwrap()
            .with_validity(Validity::from_iter(2, |index| index != 0));
        let vector = Vector::runs(vec![2, 3], values).unwrap();
        assert!(vector.validity().is_valid(0));
        assert!(vector.is_null_at(0));
        assert!(vector.is_null_at(1));
        assert!(!vector.is_null_at(2));
    }

    /// Every other form keeps its nulls in its own mask, so the two answers agree there.
    #[test]
    fn the_forms_that_hold_their_own_nulls_answer_the_same_either_way() {
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(vec![0, 7].into()))
            .unwrap()
            .with_validity(Validity::from_iter(2, |index| index != 0));
        let constant = Vector::constant(LogicalType::Integer, Value::Null, 2);
        let sequence = Vector::sequence(10, 2, 2);
        for vector in [flat, constant, sequence] {
            for row in 0..vector.len() {
                assert_eq!(vector.is_null_at(row), !vector.validity().is_valid(row));
            }
        }
    }

    #[test]
    fn flattening_a_flat_vector_is_the_same_vector() {
        let vector = integers(&[1, 2, 3]);
        assert_eq!(vector.flatten().unwrap(), vector);
    }

    /// The same answer as `flatten` and, for the vector that is already flat and owns its values,
    /// the same allocation. Asserted on the address because that is the whole claim: the values
    /// come back where they were rather than in a copy of themselves. A flatten through a borrow
    /// cannot do that, and at the top of a query it copied every column of every chunk of the
    /// result to hand back the bytes it was given.
    #[test]
    fn flattening_a_vector_that_owns_its_values_moves_them_rather_than_copying_them() {
        let vector = integers(&[1, 2, 3, 4]);
        let address = |vector: &Vector| match vector.data() {
            Some(Data::Int32(values)) => values.as_slice().as_ptr() as usize,
            _ => panic!("the layout changed under the test"),
        };
        let stored = address(&vector);
        let flat = vector.into_flat().unwrap();
        assert_eq!(address(&flat), stored, "the values moved");
        assert_eq!(
            flat.iter().collect::<Vec<_>>(),
            (1..=4).map(Value::Integer).collect::<Vec<_>>()
        );
        // And a form that is not flat is flattened, which is the case the copy is deserved in.
        let dictionary = Vector::dictionary(vec![1, 0, 1], integers(&[7, 8])).unwrap();
        let flat = dictionary.clone().into_flat().unwrap();
        assert_eq!(flat.form(), Form::Flat);
        assert_eq!(flat.iter().collect::<Vec<_>>(), dictionary.iter().collect::<Vec<_>>());
    }

    #[test]
    fn a_decimal_reads_its_width_and_scale_from_the_type_and_not_the_data() {
        let ty = LogicalType::decimal(9, 2).unwrap();
        let vector = Vector::flat(ty, Data::Int32(vec![1234].into())).unwrap();
        assert_eq!(vector.value_at(0), Value::Decimal { unscaled: 1234, width: 9, scale: 2 });
        assert_eq!(vector.value_at(0).to_string(), "12.34");
    }

    #[test]
    fn a_decimal_writes_into_whichever_of_the_four_runs_its_precision_chose() {
        // The read path worked at every width and the write path only accepted the 128 bit run, so
        // `SELECT 2.5` produced a value nothing could store. All four widths round trip now.
        for (width, scale, unscaled) in
            [(4u8, 1u8, 25i128), (9, 2, 1234), (18, 3, 123_456), (38, 4, 1_234_567)]
        {
            let ty = LogicalType::decimal(width, scale).unwrap();
            let value = Value::Decimal { unscaled, width, scale };
            let vector = Vector::from_values(ty, &[value.clone(), Value::Null]).unwrap();
            assert_eq!(vector.value_at(0), value, "a decimal of width {width}");
            assert_eq!(vector.value_at(1), Value::Null, "a null decimal of width {width}");
        }
    }

    /// The bytes a blob holds are not required to be text, and a vector of them used to refuse the
    /// ones that were not. A byte array column in a Parquet file that nothing annotated is a blob,
    /// which is what ClickHouse writes and what ten of the ClickBench queries compare against, so
    /// this is the path those take rather than a corner of the type system.
    #[test]
    fn a_blob_holds_bytes_that_are_not_text() {
        let bytes = |raw: &[u8]| Value::Blob(raw.to_vec());
        let values = [
            bytes(b"a\xffb"),
            bytes(b"\x00\x01\x02"),
            Value::Null,
            bytes(b"\xed\xa0\x80 and long enough to leave the view"),
            bytes(b""),
        ];
        let vector = Vector::from_values(LogicalType::Blob, &values).unwrap();
        for (index, value) in values.iter().enumerate() {
            assert_eq!(&vector.value_at(index), value, "row {index}");
        }
    }

    #[test]
    fn a_decimal_too_wide_for_the_run_its_type_chose_is_an_error_and_not_a_wrong_number() {
        // Only reachable by hand, since a value's width is what picked the run. Truncating here
        // would store a different number and say nothing about it.
        let ty = LogicalType::decimal(4, 1).unwrap();
        let value = Value::Decimal { unscaled: 1_000_000, width: 4, scale: 1 };
        let error = Vector::from_values(ty, &[value]).unwrap_err();
        assert!(error.to_string().contains("does not fit"), "{error}");
    }

    #[test]
    fn a_flat_vector_costs_its_values_and_a_constant_costs_one() {
        let flat = integers(&[1; 1000]);
        assert!(
            flat.footprint() >= 4000,
            "a thousand i32 are four thousand bytes: {}",
            flat.footprint()
        );
        // The forms that compute their values rather than storing them cost nothing per value,
        // which is the point of having them and is what the memory limit should see.
        let constant = Vector::constant(LogicalType::Integer, Value::Integer(1), 1_000_000);
        assert!(constant.footprint() < 200, "a constant is one value: {}", constant.footprint());
        let sequence = Vector::sequence(0, 1, 1_000_000);
        assert!(sequence.footprint() < 200, "a sequence is two numbers: {}", sequence.footprint());
    }

    #[test]
    fn a_gather_off_a_dictionary_answers_the_same_nulls_either_way_round() {
        let words = [Value::Varchar("north".into()), Value::Null, Value::Varchar("south".into())];
        let plain: Vec<Value> =
            ["north", "east", "south"].iter().map(|word| Value::Varchar((*word).into())).collect();
        let clean = Arc::new(Vector::from_values(LogicalType::Varchar, &plain).unwrap());
        let dirty = Arc::new(Vector::from_values(LogicalType::Varchar, &words).unwrap());
        let codes = vec![0, 1, 2, 0, 1, 2];
        let sources = [
            Vector::stable_dictionary(codes.clone(), Arc::clone(&clean)).unwrap(),
            Vector::stable_dictionary(codes.clone(), Arc::clone(&dirty)).unwrap(),
            Vector::stable_dictionary(codes, Arc::clone(&clean))
                .unwrap()
                .with_validity(Validity::from_run(&[true, true, false, true, true, true])),
        ];
        // What a gather says about a row has to be what the column it came out of says about the
        // row it was taken from, whichever of the two ways the nulls are reached: the mask over the
        // codes, or the value a code stands for. The fast answer is only allowed when neither has
        // any, and an index past the end is null in both readings.
        for source in &sources {
            let picks: Vec<u32> = vec![5, 0, 3, 2, 1, 99, 4];
            let taken = source.gather(&picks).unwrap();
            for (row, &pick) in picks.iter().enumerate() {
                assert_eq!(
                    taken.is_null_at(row),
                    source.is_null_at(pick as usize),
                    "row {row} of a gather of {picks:?}"
                );
            }
        }
    }

    #[test]
    fn a_dictionary_read_by_many_cuts_is_counted_about_once_between_them() {
        let strings: Vec<Value> = (0..2000)
            .map(|at| Value::Varchar(format!("a value well past the inline limit, number {at}")))
            .collect();
        let values = Arc::new(Vector::from_values(LogicalType::Varchar, &strings).unwrap());
        let dictionary = values.footprint();
        let cuts: Vec<Vector> = (0..500)
            .map(|_| Vector::stable_dictionary(vec![0; 8], Arc::clone(&values)).unwrap())
            .collect();
        let together: usize = cuts.iter().map(Vector::footprint).sum();
        // Five hundred chunks cut out of one page hold one dictionary, and what they say they hold
        // has to be about one dictionary. Before this it was five hundred of them, which is a
        // reading that grows with the answer and refuses a query holding a gigabyte a budget of
        // twenty five.
        assert!(
            together < dictionary * 2,
            "five hundred cuts are not five hundred dictionaries: {together} against {dictionary}"
        );
        assert!(
            together > dictionary / 2,
            "the dictionary is still counted: {together} against {dictionary}"
        );
    }

    #[test]
    fn a_string_vector_costs_the_bytes_of_its_long_strings() {
        let short =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("red".into())]).unwrap();
        let long = "a string well past the sixteen bytes a view holds inline".to_string();
        let spilled =
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar(long.clone())]).unwrap();
        assert!(
            spilled.footprint() >= short.footprint() + long.len(),
            "the arena is counted: {} against {}",
            spilled.footprint(),
            short.footprint()
        );
    }

    /// The cases worth checking are the widths where a code straddles a word boundary, which is
    /// every width that does not divide sixty four, and the two ends of the range.
    #[test]
    fn a_narrow_column_packs_and_reads_back_the_same_at_every_width() {
        for width in 1..=20u32 {
            let span = (1i64 << width) - 1;
            let values: Vec<i64> =
                (0..1000).map(|row| 1_000_000 + (row * 7919) % (span + 1)).collect();
            let flat =
                Vector::flat(LogicalType::BigInt, Data::Int64(values.clone().into())).unwrap();
            let packed = flat.bit_packed().unwrap();
            assert_eq!(packed.len(), flat.len());
            assert_eq!(
                packed.iter().collect::<Vec<_>>(),
                flat.iter().collect::<Vec<_>>(),
                "width {width} read back differently"
            );
        }
    }

    #[test]
    fn the_width_is_the_bits_the_range_needs_and_not_the_bits_the_type_has() {
        let values: Vec<i32> = (0..1024).map(|row| 40 + (row * 2560) / 1023).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        let packed = flat.bit_packed().unwrap();
        assert_eq!(packed.form(), Form::BitPacked);
        let parts = packed.packed_parts().expect("packed");
        assert_eq!(parts.width(), 12, "0 to 2560 is twelve bits");
        assert_eq!(parts.base(), 40);
        assert!(
            packed.footprint() * 2 < flat.footprint(),
            "twelve bits against thirty two: {} against {}",
            packed.footprint(),
            flat.footprint()
        );
    }

    /// The check is worth having in both directions, the way the run length one is. A form that is
    /// only ever bigger than what it replaced costs a pass over the column to decide not to use.
    #[test]
    fn a_column_that_uses_its_whole_type_is_left_flat() {
        let values: Vec<i32> = (0..1024).map(|row| row * 2_000_000 - 1_000_000_000).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        assert_eq!(flat.bit_packed().unwrap().form(), Form::Flat);
    }

    /// The column that would not write. A thousand values just under `i32::MAX` need ten bits, and
    /// based at the smallest of them those ten bits could say a number an `INTEGER` cannot hold, so
    /// the range check refused the column and `CREATE TABLE` came back with an internal error. The
    /// base is what moves, not the check: it drops to where the widest code the width allows is the
    /// largest value the type has.
    #[test]
    fn a_column_against_the_top_of_its_type_packs_rather_than_being_refused() {
        let values: Vec<i32> = (0..4096).map(|row| i32::MAX - (row % 1000)).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.clone().into())).unwrap();
        let packed = flat.bit_packed().unwrap();
        assert_eq!(packed.form(), Form::BitPacked);
        let parts = packed.packed_parts().expect("packed");
        assert_eq!(parts.width(), 10, "a thousand values apart is ten bits");
        assert_eq!(
            parts.base() + i128::from(u64::MAX >> (64 - parts.width())),
            i128::from(i32::MAX),
            "the widest code the width allows is the largest value the type holds"
        );
        assert_eq!(
            packed.iter().collect::<Vec<_>>(),
            flat.iter().collect::<Vec<_>>(),
            "the values came back different"
        );
    }

    /// The other end of the same thing. A column that reaches both ends of its type needs every bit
    /// the type has, and the only base that leaves room for those codes is the bottom of the type.
    #[test]
    fn a_column_that_reaches_both_ends_of_its_type_bases_at_the_bottom_of_it() {
        let values: Vec<i32> = (0..4096)
            .map(|row| if row % 2 == 0 { i32::MIN + row } else { i32::MAX - row })
            .collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.clone().into())).unwrap();
        // Thirty two bits of codes for a thirty two bit type buys nothing, so the size check leaves
        // it flat. What matters is that it is left flat rather than refused.
        assert_eq!(flat.bit_packed().unwrap().form(), Form::Flat);
        assert_eq!(
            packing_base(&LogicalType::Integer, i128::from(i32::MIN), i128::from(i32::MAX), 32),
            Some(i128::from(i32::MIN))
        );
    }

    /// A column of one value would pack to no bits at all, and one run is smaller than any packing
    /// of it, so the two forms do not fight over that column.
    #[test]
    fn a_column_of_one_value_is_left_to_the_run_length_form() {
        let flat = integers(&[9; 1024]);
        assert_eq!(flat.bit_packed().unwrap().form(), Form::Flat);
        assert_eq!(flat.run_encoded().unwrap().form(), Form::Rle);
    }

    #[test]
    fn a_string_column_has_no_range_to_pack() {
        let text = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("blue".into())],
        )
        .unwrap();
        assert_eq!(text.bit_packed().unwrap().form(), Form::Flat);
    }

    /// The cut is the reason the form carries a row to start reading at. It stays packed, it shares
    /// the same words, and it reads the rows the range asked for.
    #[test]
    fn a_cut_of_a_packed_column_stays_packed_and_shares_its_bits() {
        let values: Vec<i32> = (0..1024).map(|row| 100 + row % 300).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        let packed = flat.bit_packed().unwrap();
        let cut = packed.slice(500, 24).unwrap();
        assert_eq!(cut.form(), Form::BitPacked);
        assert_eq!(cut.len(), 24);
        assert_eq!(
            cut.iter().collect::<Vec<_>>(),
            flat.slice(500, 24).unwrap().iter().collect::<Vec<_>>()
        );
        assert!(
            cut.footprint() >= packed.footprint(),
            "a cut shares the words rather than copying a piece of them"
        );
    }

    #[test]
    fn a_gather_of_a_packed_column_comes_out_flat_and_keeps_the_nulls() {
        let values: Vec<i32> = (0..64).map(|row| 10 + row).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        let packed =
            flat.bit_packed().unwrap().with_validity(Validity::from_iter(64, |row| row % 3 != 0));
        let taken = packed.gather(&[0, 1, 2, 3, 62]).unwrap();
        assert_eq!(taken.form(), Form::Flat);
        assert_eq!(
            taken.iter().collect::<Vec<_>>(),
            vec![
                Value::Null,
                Value::Integer(11),
                Value::Integer(12),
                Value::Null,
                Value::Integer(72)
            ]
        );
    }

    /// The pair a comparison kernel asks for before it reads a bit. A literal inside the range has a
    /// code and a literal outside it does not, which answers the whole vector at once.
    #[test]
    fn a_literal_outside_the_packed_range_has_no_code() {
        let values: Vec<i32> = (0..256).map(|row| 1000 + row).collect();
        let flat = Vector::flat(LogicalType::Integer, Data::Int32(values.into())).unwrap();
        let packed = flat.bit_packed().unwrap();
        let parts = packed.packed_parts().expect("packed");
        assert_eq!(parts.code_of(1000), Some(0));
        assert_eq!(parts.code_of(1100), Some(100));
        assert_eq!(parts.code_of(999), None);
        assert!(parts.ceiling() >= 1255);
        assert_eq!(parts.code_of(parts.ceiling() + 1), None);
    }

    /// The bits arriving from a file rather than from a flat vector, which is what the form is for.
    #[test]
    fn packed_bits_can_be_handed_in_without_a_flat_vector_to_start_from() {
        let packed = Vector::packed(LogicalType::SmallInt, vec![0x0000_0000_0000_4321], 4, 7, 4)
            .expect("four codes of four bits");
        assert_eq!(
            packed.iter().collect::<Vec<_>>(),
            vec![Value::SmallInt(8), Value::SmallInt(9), Value::SmallInt(10), Value::SmallInt(11)]
        );
    }

    #[test]
    fn packed_bits_that_could_not_hold_what_they_claim_are_refused() {
        assert!(Vector::packed(LogicalType::Varchar, vec![0], 4, 0, 4).is_err(), "not an integer");
        assert!(Vector::packed(LogicalType::Integer, vec![0], 0, 0, 4).is_err(), "no width");
        assert!(Vector::packed(LogicalType::Integer, vec![0], 64, 0, 4).is_err(), "too wide");
        assert!(Vector::packed(LogicalType::Integer, vec![0], 8, 0, 9).is_err(), "too few words");
        assert!(Vector::packed(LogicalType::TinyInt, vec![0], 8, 100, 8).is_err(), "would not fit");
    }

    /// A column of strings long enough that the payload is in the arena rather than in the views.
    fn long_strings(count: usize) -> Vector {
        let values: Vec<Value> = (0..count)
            .map(|row| {
                Value::Varchar(format!("a string too long to sit inside a view, number {row}"))
            })
            .collect();
        Vector::from_values(LogicalType::Varchar, &values).unwrap()
    }

    #[test]
    fn a_string_column_in_view_form_reads_back_the_same_strings() {
        let flat = long_strings(40);
        let shared = flat.clone().shared_text().unwrap();
        assert_eq!(shared.form(), Form::StringView);
        assert_eq!(shared.len(), 40);
        for row in 0..40 {
            assert_eq!(shared.value_at(row), flat.value_at(row), "row {row}");
            assert_eq!(shared.text_at(row), flat.text_at(row), "row {row}");
        }
    }

    #[test]
    fn a_short_string_is_read_out_of_its_view_and_never_out_of_the_arena() {
        let flat = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("red".into()), Value::Varchar("green".into()), Value::Null],
        )
        .unwrap();
        let shared = flat.shared_text().unwrap();
        // Nothing went to the arena, so the whole column resolves with an empty one.
        let (views, arena) = shared.text_parts().unwrap();
        assert!(arena.is_empty(), "three short strings need no arena");
        assert_eq!(views[0].bytes_in(arena), Some(&b"red"[..]));
        assert_eq!(shared.value_at(1), Value::Varchar("green".into()));
        assert_eq!(shared.value_at(2), Value::Null, "the validity came across");
    }

    #[test]
    fn a_cut_of_a_view_column_shares_the_arena_rather_than_copying_the_bytes() {
        let shared = long_strings(64).shared_text().unwrap();
        let cut = shared.slice(16, 8).unwrap();
        assert_eq!(cut.form(), Form::StringView, "a cut of views is views");
        assert_eq!(cut.len(), 8);
        assert_eq!(cut.value_at(0), shared.value_at(16));
        assert_eq!(cut.value_at(7), shared.value_at(23));
        // The arena is the same bytes at the same address, which is the whole point of the form.
        let (_, whole) = shared.text_parts().unwrap();
        let (_, piece) = cut.text_parts().unwrap();
        assert_eq!(piece.as_ptr(), whole.as_ptr(), "the cut shares the page");
        assert_eq!(piece.len(), whole.len());
    }

    #[test]
    fn a_flat_string_column_has_to_copy_the_bytes_its_cut_keeps() {
        let flat = long_strings(64);
        let cut = flat.slice(16, 8).unwrap();
        assert_eq!(cut.form(), Form::Flat);
        let (_, whole) = flat.text_parts().unwrap();
        let (_, piece) = cut.text_parts().unwrap();
        assert!(piece.len() < whole.len(), "the flat cut carries only what it kept");
    }

    #[test]
    fn a_gather_of_a_view_column_keeps_the_form_and_a_flatten_copies_out_of_it() {
        let shared = long_strings(32).shared_text().unwrap();
        let picked: Vec<u32> = (0..32).step_by(3).collect();
        let gathered = shared.gather(&picked).unwrap();
        assert_eq!(gathered.form(), Form::StringView, "selecting rows moves views, not bytes");
        assert_eq!(gathered.len(), picked.len());
        for (row, &from) in picked.iter().enumerate() {
            assert_eq!(gathered.value_at(row), shared.value_at(from as usize), "row {row}");
        }
        let flattened = gathered.flatten().unwrap();
        assert_eq!(flattened.form(), Form::Flat);
        assert_eq!(flattened.iter().collect::<Vec<_>>(), gathered.iter().collect::<Vec<_>>());
        // The flatten is what narrows the bytes, so the arena it built holds only the rows it kept.
        let (_, narrowed) = flattened.text_parts().unwrap();
        let (_, whole) = shared.text_parts().unwrap();
        assert!(narrowed.len() < whole.len(), "flattening lets the page go");
    }

    #[test]
    fn a_null_in_a_view_column_survives_being_gathered_and_flattened() {
        let shared = long_strings(8)
            .with_validity(Validity::from_iter(8, |row| row % 3 != 0))
            .shared_text()
            .unwrap();
        let gathered = shared.gather(&[0, 1, 2, 3, 4]).unwrap();
        let expected =
            [Value::Null, shared.value_at(1), shared.value_at(2), Value::Null, shared.value_at(4)];
        assert_eq!(gathered.iter().collect::<Vec<_>>(), expected);
        assert_eq!(gathered.flatten().unwrap().iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn both_string_forms_hand_a_kernel_the_same_views_and_the_same_bytes() {
        let flat = long_strings(6);
        let shared = flat.clone().shared_text().unwrap();
        let (flat_views, flat_arena) = flat.text_parts().unwrap();
        let (shared_views, shared_arena) = shared.text_parts().unwrap();
        assert_eq!(flat_views.len(), shared_views.len());
        for row in 0..6 {
            assert_eq!(
                flat_views[row].bytes_in(flat_arena),
                shared_views[row].bytes_in(shared_arena),
                "row {row}"
            );
        }
        // Nothing else answers this, which is what keeps a kernel from taking it for a string column.
        assert!(Vector::sequence(0, 1, 4).text_parts().is_none());
        assert!(integers(&[1, 2, 3]).text_parts().is_none());
    }

    #[test]
    fn a_column_that_is_not_strings_cannot_be_held_as_views() {
        let views = vec![StringView::inline("red")];
        let arena = Arc::new(Buffer::new());
        let wrong = Vector::string_views(LogicalType::Integer, views, arena);
        assert!(wrong.is_err(), "an integer column has no views");
        assert_eq!(integers(&[1, 2]).shared_text().unwrap().form(), Form::Flat, "left alone");
    }

    /// A column with enough repeated structure for a symbol table to find something, which is what
    /// a real text column has and a column of random bytes does not.
    fn sentences(count: usize) -> Vector {
        let values: Vec<Value> = (0..count)
            .map(|row| {
                Value::Varchar(format!(
                    "http://example.test/catalogue/section/{}/item/{row}",
                    row % 7
                ))
            })
            .collect();
        Vector::from_values(LogicalType::Varchar, &values).unwrap()
    }

    #[test]
    fn a_compressed_column_reads_back_the_strings_that_went_into_it() {
        let flat = sentences(64);
        let coded = flat.clone().compressed().unwrap();
        assert_eq!(coded.form(), Form::Fsst, "a text column compresses");
        assert_eq!(coded.len(), 64);
        for row in 0..64 {
            assert_eq!(coded.value_at(row), flat.value_at(row), "row {row}");
        }
        assert_eq!(coded.flatten().unwrap(), flat, "flattening is the column it came from");
    }

    #[test]
    fn compressing_halves_the_bytes_or_the_column_is_left_flat() {
        let flat = sentences(200);
        let coded = flat.clone().compressed().unwrap();
        let parts = coded.coded_parts().expect("compressed");
        // Read through the flat column, because the compressed one has no bytes to hand back where
        // they are and answers `None` to `text_at` rather than decompressing into a borrow.
        assert_eq!(coded.text_at(0), None, "nothing to borrow until it is flattened");
        let plain: usize = (0..200).map(|row| flat.text_at(row).map_or(0, str::len)).sum();
        let codes: usize = (0..200).map(|row| parts.row(row).map_or(0, <[u8]>::len)).sum();
        assert!(codes * FSST_PAYS_AT <= plain, "{codes} codes against {plain} bytes");
        // Text with no repeated structure in it gives a table nothing longer than a byte to find,
        // so the codes are the bytes and the column stays where it is rather than paying a
        // decompression per read to save nothing.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let values: Vec<Value> = (0..256)
            .map(|_| {
                let mut text = String::new();
                while text.len() < 12 {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    text.push(char::from(b'!' + ((seed >> 33) % 90) as u8));
                }
                Value::Varchar(text)
            })
            .collect();
        let noise = Vector::from_values(LogicalType::Varchar, &values).unwrap();
        assert_eq!(noise.compressed().unwrap().form(), Form::Flat);
    }

    #[test]
    fn a_cut_of_a_compressed_column_shares_the_codes_and_the_table() {
        let coded = sentences(64).compressed().unwrap();
        let cut = coded.slice(8, 16).unwrap();
        assert_eq!(cut.form(), Form::Fsst);
        assert_eq!(cut.len(), 16);
        for row in 0..16 {
            assert_eq!(cut.value_at(row), coded.value_at(8 + row), "row {row}");
        }
        let (whole, piece) = (coded.coded_parts().unwrap(), cut.coded_parts().unwrap());
        assert_eq!(piece.row(0), whole.row(8), "the spans point into the same codes");
    }

    #[test]
    fn a_gather_of_a_compressed_column_stays_compressed_and_keeps_the_nulls() {
        let coded = sentences(32)
            .with_validity(Validity::from_iter(32, |row| row % 5 != 2))
            .compressed()
            .unwrap();
        let picked: Vec<u32> = (0..32).step_by(2).collect();
        let gathered = coded.gather(&picked).unwrap();
        assert_eq!(gathered.form(), Form::Fsst, "selecting rows moves spans, not bytes");
        for (row, &from) in picked.iter().enumerate() {
            assert_eq!(gathered.value_at(row), coded.value_at(from as usize), "row {row}");
        }
        assert_eq!(
            gathered.flatten().unwrap().iter().collect::<Vec<_>>(),
            gathered.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_literal_lands_in_the_same_codes_the_row_holding_it_does() {
        let coded = sentences(40).compressed().unwrap();
        let parts = coded.coded_parts().expect("compressed");
        let text = coded.value_at(11);
        let Value::Varchar(text) = text else { panic!("a string column reads back strings") };
        assert_eq!(parts.encode(text.as_bytes()), parts.row(11).expect("row 11"));
        assert_ne!(parts.encode(b"something else entirely"), parts.row(11).unwrap());
    }

    #[test]
    fn codes_that_run_past_what_is_there_are_refused() {
        let table = Arc::new(SymbolTable::empty());
        let codes = Arc::new(vec![1u8, 2, 3, 4]);
        let good = vec![(0u32, 2u32), (2, 4)];
        assert!(
            Vector::coded(LogicalType::Varchar, Arc::clone(&codes), good, Arc::clone(&table))
                .is_ok()
        );
        let past = vec![(0u32, 9u32)];
        assert!(
            Vector::coded(LogicalType::Varchar, Arc::clone(&codes), past, Arc::clone(&table))
                .is_err(),
            "a span past the end of the codes"
        );
        let backwards = vec![(3u32, 1u32)];
        assert!(
            Vector::coded(LogicalType::Varchar, Arc::clone(&codes), backwards, Arc::clone(&table))
                .is_err(),
            "a span that ends before it starts"
        );
        let wrong = vec![(0u32, 2u32)];
        assert!(
            Vector::coded(LogicalType::Integer, codes, wrong, table).is_err(),
            "an integer column has no codes"
        );
    }

    #[test]
    fn a_view_pointing_past_its_arena_is_refused_at_construction() {
        let long = "a string too long to sit inside a view";
        let arena: Arc<Buffer<u8>> = Arc::new(long.as_bytes().to_vec().into());
        let good = vec![StringView::over(long.as_bytes(), 0)];
        assert!(Vector::string_views(LogicalType::Varchar, good, Arc::clone(&arena)).is_ok());
        let bad = vec![StringView::over(long.as_bytes(), 4)];
        assert!(
            Vector::string_views(LogicalType::Varchar, bad, arena).is_err(),
            "four bytes short of what the view claims"
        );
    }

    /// The form at its simplest: an id per row, and the row it names.
    #[test]
    fn a_gathered_vector_reads_the_source_row_its_id_names() {
        let source = Arc::new(integers(&[10, 20, 30, 40]));
        let vector = Vector::gathered(source, Arc::new(vec![3, 0, 3, 1])).unwrap();
        assert_eq!(vector.form(), Form::Gathered);
        assert_eq!(vector.len(), 4);
        assert_eq!(
            vector.iter().collect::<Vec<_>>(),
            vec![Value::Integer(40), Value::Integer(10), Value::Integer(40), Value::Integer(20)]
        );
    }

    /// Section 8.2's lazy validity. The sentinel is a null and it is not in a mask anywhere, which is
    /// what lets a left link join gather null for an unmatched child row without allocating one.
    #[test]
    fn a_gathered_row_with_no_source_row_is_null_without_a_mask() {
        let source = Arc::new(integers(&[10, 20]));
        let vector = Vector::gathered(source, Arc::new(vec![1, NO_ROW, 0])).unwrap();
        assert!(!vector.validity().has_nulls(vector.len()), "the mask at this level says nothing");
        assert!(vector.is_null_at(1));
        assert!(!vector.is_null_at(0) && !vector.is_null_at(2));
        assert_eq!(
            vector.iter().collect::<Vec<_>>(),
            vec![Value::Integer(20), Value::Null, Value::Integer(10)]
        );
        assert!(!vector.none_null(), "a sentinel is a null and the bulk answer has to agree");
    }

    /// The other half of the same rule: a null in the source is a null here, the way a dictionary's
    /// nulls live in its values. Two ways for a row to be null and one answer from `is_null_at`.
    #[test]
    fn a_gather_of_a_null_source_row_is_null() {
        let source = Arc::new(
            Vector::from_values(LogicalType::Integer, &[Value::Integer(7), Value::Null]).unwrap(),
        );
        let vector = Vector::gathered(source, Arc::new(vec![1, 0, 1])).unwrap();
        assert!(vector.is_null_at(0) && vector.is_null_at(2));
        assert_eq!(vector.value_at(1), Value::Integer(7));
        assert!(!vector.none_null());
    }

    /// An id past the end of the source is the one failure in this form that reads whatever happens
    /// to be at that offset rather than failing, so it is refused where the vector is built.
    #[test]
    fn a_gathered_id_past_the_end_of_its_source_is_refused() {
        let source = Arc::new(integers(&[1, 2, 3]));
        assert!(Vector::gathered(Arc::clone(&source), Arc::new(vec![0, 3])).is_err());
        assert!(
            Vector::gathered(source, Arc::new(vec![0, NO_ROW])).is_ok(),
            "the sentinel is not an id past the end, it is the absence of one"
        );
    }

    /// A cut is the offset and nothing else, which is what keeps a pipeline from copying the ids once
    /// per operator. Both ends stay shared and the rows answer the same.
    #[test]
    fn cutting_a_gather_moves_where_it_starts_and_copies_nothing() {
        let source = Arc::new(integers(&[10, 20, 30, 40, 50]));
        let rids = Arc::new(vec![4, 3, 2, 1, 0]);
        let vector = Vector::gathered(Arc::clone(&source), Arc::clone(&rids)).unwrap();
        let held = Arc::strong_count(&rids);
        let cut = vector.slice(1, 3).unwrap();
        assert_eq!(cut.form(), Form::Gathered);
        assert_eq!(
            Arc::strong_count(&rids),
            held + 1,
            "the cut shares the ids rather than copying"
        );
        assert_eq!(
            cut.iter().collect::<Vec<_>>(),
            vec![Value::Integer(40), Value::Integer(30), Value::Integer(20)]
        );
        assert_eq!(cut.gathered_parts().unwrap().1, [3, 2, 1]);
    }

    /// Composition, which is why this is a body and not an operator. A filter over the output of a
    /// link join selects into the ids, and what comes out is one level rather than two.
    #[test]
    fn a_gather_of_a_gather_resolves_to_one_walk_over_the_source() {
        let source = Arc::new(integers(&[10, 20, 30, 40]));
        let inner = Vector::gathered(source, Arc::new(vec![3, 2, 1, 0])).unwrap();
        let outer = inner.gather(&[0, 3]).unwrap();
        assert_eq!(outer.iter().collect::<Vec<_>>(), vec![Value::Integer(40), Value::Integer(10)]);
        assert_ne!(outer.form(), Form::Gathered, "the walk stops at what the ids point into");
    }

    /// The sentinel survives being gathered through, which it has to: a filter over a left link
    /// join's output keeps the unmatched rows it kept and they are still null.
    #[test]
    fn gathering_through_a_sentinel_keeps_it_null() {
        let source = Arc::new(integers(&[10, 20]));
        let inner = Vector::gathered(source, Arc::new(vec![0, NO_ROW, 1])).unwrap();
        let outer = inner.gather(&[1, 2, 1]).unwrap();
        assert_eq!(
            outer.iter().collect::<Vec<_>>(),
            vec![Value::Null, Value::Integer(20), Value::Null]
        );
    }

    /// Section 8.2's dispatch rule, which is the whole difference between this form and a dictionary
    /// and is one comparison. A gather off a parent larger than the chunk does not want the
    /// dictionary arm of any kernel, and a gather off a source smaller than the chunk does.
    #[test]
    fn folding_over_the_source_is_worth_it_only_when_the_source_is_the_shorter_one() {
        let wide = Arc::new(integers(&(0..64).collect::<Vec<i32>>()));
        let narrow = Arc::new(integers(&[1, 2]));
        let off_wide = Vector::gathered(wide, Arc::new(vec![0, 1, 2])).unwrap();
        let off_narrow = Vector::gathered(narrow, Arc::new(vec![0, 1, 0, 1, 0])).unwrap();
        assert!(!off_wide.fold_over_source(), "sixty four source rows to answer three");
        assert!(off_narrow.fold_over_source(), "two source rows to answer five");
        assert!(!integers(&[1, 2]).fold_over_source(), "and every other form says no");
    }

    /// Strings, which read their bytes where the source already has them rather than through a value.
    /// A gather of a string column is four bytes a row and no arena is touched until something asks.
    #[test]
    fn a_gathered_string_is_read_where_the_source_put_it() {
        let mut column = StringColumn::new();
        column.push("red");
        column.push("a string too long to sit inside a sixteen byte view");
        let source = Arc::new(Vector::flat(LogicalType::Varchar, Data::Varlen(column)).unwrap());
        let vector = Vector::gathered(source, Arc::new(vec![1, 0, NO_ROW])).unwrap();
        assert_eq!(vector.text_at(0), Some("a string too long to sit inside a sixteen byte view"));
        assert_eq!(vector.text_at(1), Some("red"));
        assert_eq!(vector.text_at(2), None);
        assert_eq!(vector.bytes_at(1), Some(b"red".as_slice()));
        assert_eq!(vector.value_at(1), Value::Varchar("red".into()));
    }

    /// The integer accessor a group by keys through, which has to agree with `value_at` at every
    /// row or two rows holding one value land in two groups.
    #[test]
    fn the_signed_reader_of_a_gather_agrees_with_the_value_reader() {
        let source = Arc::new(integers(&[10, 20, 30]));
        let vector = Vector::gathered(source, Arc::new(vec![2, NO_ROW, 0, 1])).unwrap();
        for row in 0..vector.len() {
            let signed = vector.signed_at(row);
            match vector.value_at(row) {
                Value::Null => assert_eq!(signed, None),
                Value::Integer(held) => assert_eq!(signed, Some(i128::from(held))),
                other => panic!("an integer column answered {other}"),
            }
        }
    }

    /// Flattening gives up the form, which is what it is for, and what comes out holds the values the
    /// gather stood for, nulls included.
    #[test]
    fn flattening_a_gather_writes_out_the_rows_it_pointed_at() {
        let source = Arc::new(integers(&[10, 20, 30]));
        let vector = Vector::gathered(source, Arc::new(vec![2, NO_ROW, 0])).unwrap();
        let flat = vector.flatten().unwrap();
        assert_eq!(flat.form(), Form::Flat);
        assert_eq!(
            flat.iter().collect::<Vec<_>>(),
            vec![Value::Integer(30), Value::Null, Value::Integer(10)]
        );
    }

    /// A gather counts a share of what it shares, for the reason a dictionary does. Eight columns
    /// gathered off one parent are one parent between them, not eight.
    #[test]
    fn a_parent_gathered_by_many_columns_is_counted_about_once_between_them() {
        let source = Arc::new(integers(&(0..4096).collect::<Vec<i32>>()));
        let rids = Arc::new(vec![0; 64]);
        let alone = Vector::gathered(Arc::clone(&source), Arc::clone(&rids)).unwrap().footprint();
        let many = (0..8)
            .map(|_| Vector::gathered(Arc::clone(&source), Arc::clone(&rids)).unwrap())
            .collect::<Vec<_>>();
        let together = many.iter().map(Vector::footprint).sum::<usize>();
        assert!(
            together < alone * 2,
            "eight gathers off one parent reported {together} against {alone} for one"
        );
    }
}
