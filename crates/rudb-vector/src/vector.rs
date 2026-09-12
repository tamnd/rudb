//! The vector itself.
//!
//! `spec/07-execution.md` section 7.1 calls this the widest interface in the system, says every
//! operator depends on it, and says changing it after twenty operators exist is expensive. So it
//! is written before the first operator rather than after the fifth.
//!
//! A vector is a type, a length of at most [`VECTOR_SIZE`], a physical form, a validity
//! representation and some data. Four of the forms are the ones in `spec/04-architecture.md`
//! section 4.3: flat, constant, sequence and dictionary. Run length is the fifth and it is the first
//! of the encoded ones, which arrive one at a time with the kernels that read them rather than all
//! at once ahead of anything that can use them.
//!
//! Dictionary and run length are the pair worth understanding together, because they answer
//! different questions about the same column. A dictionary says which distinct values there are, so
//! it wins on low cardinality however the rows are ordered. Run length says where the values stop,
//! so it wins on a clustered column however many distinct values it has. A column can want either
//! one without wanting the other, and `hits` has columns of both kinds.
//!
//! **What is not here yet.** Buffers are owned. Section 7.1 says a vector borrowed from a buffer
//! managed page carries a pin, and there is no buffer manager until M2, so there is nothing to pin
//! and pretending otherwise would be an interface built against an imaginary caller. Nested types
//! are not stored yet either, for the same reason: a `LIST(STRUCT(...))` is offsets plus child
//! column chunks, and child column chunks are storage.

use std::borrow::Cow;
use std::sync::Arc;

use rudb_common::{Cause, Error, LogicalType, Result, Value, slow};

use crate::buffer::Buffer;
use crate::string::{StringColumn, StringView};
use crate::validity::Validity;

/// How many values are in a full vector.
///
/// 1024 rather than DuckDB's 2048, per `spec/04-architecture.md` section 4.3. It is the FastLanes
/// unit, it makes a validity mask exactly 16 `u64` words, and it keeps a vector of 16 byte string
/// views at 16 KiB, which is the size at which several of these fit in L1 together rather than
/// evicting each other.
pub const VECTOR_SIZE: usize = 1024;

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
    /// One value per run, with the row each run ends at.
    ///
    /// The form a clustered column is in. `hits` is written in time order, so `EventDate` is a few
    /// hundred runs over a hundred million rows, and a sum over it is a few hundred multiplications
    /// rather than a hundred million additions. Dictionary says which distinct values there are and
    /// this says where they stop, and a column can want either one without wanting the other.
    Rle,
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
        codes: Vec<u32>,
        values: Arc<Vector>,
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
    /// If a value is not one the type can hold, or if the type is one that cannot be stored flat
    /// yet, which today means the nested types.
    pub fn from_values(ty: LogicalType, values: &[Value]) -> Result<Self> {
        let mut data = empty_data_for(&ty)?;
        for value in values {
            push_value(&mut data, value)?;
        }
        let validity = Validity::from_iter(values.len(), |index| !values[index].is_null());
        Ok(Self { ty, len: values.len(), validity, body: Body::Flat(data) })
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
        if let Some(&bad) = codes.iter().find(|&&code| code as usize >= values.len()) {
            return Err(Error::internal(format!(
                "dictionary code {bad} is past the end of a {} value dictionary",
                values.len()
            )));
        }
        let (codes, values) = compose(codes, values);
        Ok(Self {
            ty: values.ty.clone(),
            len: codes.len(),
            validity: Validity::AllValid,
            body: Body::Dictionary { codes, values: Arc::new(values) },
        })
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
    /// If the type has no flat layout, which today means the nested types.
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
        if words_for(self.len, width) * size_of::<u64>() * PACKING_PAYS_AT > data.footprint() {
            return Ok(self.clone());
        }
        let words = pack(data, self.len, low, width);
        let packed = Self::packed(self.ty.clone(), words, width, low, self.len)?;
        Ok(packed.with_validity(self.validity.clone()))
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
    /// A dictionary counts its values in full, and two vectors sharing one dictionary each report
    /// all of it. That over counts, deliberately: working out that two operators are looking at the
    /// same `Arc` means threading identity through the accounting, and a limit that over counts
    /// refuses a query that would have fit while a limit that under counts lets one through that
    /// does not. The first is a worse answer to give and the second is a worse thing to be.
    #[must_use]
    pub fn footprint(&self) -> usize {
        let body = match &self.body {
            Body::Flat(data) => data.footprint(),
            Body::Constant(value) => value.footprint(),
            Body::Sequence { .. } => 0,
            Body::Dictionary { codes, values } => {
                codes.capacity() * size_of::<u32>() + values.footprint()
            }
            Body::Packed { words, .. } => words.capacity() * size_of::<u64>(),
            Body::Runs { ends, values } => ends.capacity() * size_of::<u32>() + values.footprint(),
        };
        size_of::<Self>() + self.validity.footprint() + body
    }

    /// Which of the values are not null.
    #[must_use]
    pub fn validity(&self) -> &Validity {
        &self.validity
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
            Body::Runs { .. } => Form::Rle,
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
            Body::Dictionary { codes, values } => Some((codes, values.as_ref())),
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
            Body::Dictionary { codes, values } => Some((Cow::Borrowed(codes), values.as_ref())),
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
            Body::Dictionary { codes, values } => match codes.get(index) {
                Some(&code) => values.value_at(code as usize),
                None => Value::Null,
            },
            Body::Runs { ends, values } => match run_holding(ends, index) {
                Some(run) => values.value_at(run),
                None => Value::Null,
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
            Body::Flat(data) => value_from(&self.ty, data, index),
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
            Body::Dictionary { codes, values } => {
                values.text_at(usize::try_from(*codes.get(index)?).ok()?)
            }
            Body::Runs { ends, values } => values.text_at(run_holding(ends, index)?),
            _ => None,
        }
    }

    /// Every value in order, as single values.
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.len).map(|index| self.value_at(index))
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
    /// and a flat body is the one that genuinely has to copy its range.
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
        let validity = Validity::from_iter(len, |row| self.validity.is_valid(at + row));
        let body = match &self.body {
            Body::Constant(value) => Body::Constant(value.clone()),
            Body::Sequence { start, step } => {
                Body::Sequence { start: start + step * at as i64, step: *step }
            }
            Body::Dictionary { codes, values } => {
                Body::Dictionary { codes: codes[at..end].to_vec(), values: Arc::clone(values) }
            }
            // The bits are not byte aligned, so a cut either repacks them or moves the row the
            // reading starts at. Moving it is one addition and repacking is a pass, and a page is
            // cut into chunk sized pieces often enough that the difference is the form.
            Body::Packed { words, width, base, offset } => Body::Packed {
                words: Arc::clone(words),
                width: *width,
                base: *base,
                offset: offset + at,
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
            // The one form with nowhere to point, so its range is copied out. A gather is the
            // right tool here and does no more than this would: a flat body has no dictionary
            // under it for the gather to flatten.
            Body::Flat(_) => {
                let indices: Vec<u32> =
                    (at..end).map(|row| u32::try_from(row).unwrap_or(u32::MAX)).collect();
                return self.gather(&indices);
            }
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
    /// If the type is one this crate cannot store flat yet, which today means the nested types.
    pub fn flatten(&self) -> Result<Self> {
        if let Body::Flat(_) = self.body {
            return Ok(self.clone());
        }
        slow::took(Cause::Flatten);
        self.copied((0..self.len).collect(), false)
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
    /// If the type has no flat layout, which today means the nested types.
    pub fn gather(&self, indices: &[u32]) -> Result<Self> {
        self.copied(indices.iter().map(|&index| index as usize).collect(), true)
    }

    /// The copy both [`Self::gather`] and [`Self::flatten`] are.
    ///
    /// `constants_stay` is the one thing the two want differently. A gather of a constant is a
    /// shorter constant and copying it out would be a thousand writes of the same value for nothing,
    /// but flattening promises flat form to a caller that is about to read the data slice, so for
    /// that one the constant has to be written out.
    fn copied(&self, at: Vec<usize>, constants_stay: bool) -> Result<Self> {
        let rows = at.len();
        let (at, leaf) = self.resolve(at);
        let live: Vec<bool> = at.iter().map(|&index| index != NOWHERE).collect();
        let validity = Validity::from_run(&live);
        let body = match &leaf.body {
            // Every position holds the same value, so the only thing the gather can change is the
            // length and which positions are null. A gather with no null in it is still a constant.
            Body::Constant(value) => {
                if constants_stay && matches!(validity, Validity::AllValid) {
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
            // Unreachable, because `resolve` walks past both of the forms that point at another
            // vector and stops at the first body that does not.
            Body::Dictionary { .. } | Body::Runs { .. } => {
                return Err(Error::internal(
                    "a form that points somewhere survived being resolved",
                ));
            }
        };
        Ok(Self { ty: self.ty.clone(), len: rows, validity, body })
    }

    /// Where each wanted position lives in the first body that is not a dictionary, and that body.
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
                Body::Dictionary { codes, values } => {
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
    #[must_use]
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
                    let mut low = i128::MAX;
                    let mut high = i128::MIN;
                    for &value in values.as_slice().iter().take(len) {
                        let value = i128::from(value);
                        low = low.min(value);
                        high = high.max(value);
                    }
                    (low <= high).then_some((low, high))
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
fn compose(codes: Vec<u32>, values: Vector) -> (Vec<u32>, Vector) {
    // A dictionary carrying a validity of its own is one whose nulls live at this level rather than
    // in the values, which is the one thing composition cannot carry down with it.
    if !matches!(values.validity, Validity::AllValid) {
        return (codes, values);
    }
    let Vector { ty, len, validity, body } = values;
    match body {
        Body::Dictionary { codes: inner, values: leaf } => {
            debug_assert!(
                !matches!(leaf.body, Body::Dictionary { .. })
                    || !matches!(leaf.validity, Validity::AllValid),
                "a dictionary was stacked on a dictionary without going through the constructor"
            );
            // The leaf is shared, so taking it out of the `Arc` copies it when something else is
            // still holding the same dictionary. That is the rare path: a dictionary over a
            // dictionary only arrives from a caller that built one that way, and the cut that made
            // sharing worth doing produces neither.
            (codes.iter().map(|&code| inner[code as usize]).collect(), Arc::unwrap_or_clone(leaf))
        }
        body => (codes, Vector { ty, len, validity, body }),
    }
}

/// How many rows a run has to cover on average before run length encoding is smaller.
///
/// A run costs its value plus the four bytes of its end, so on a four byte column a run of two rows
/// breaks even and a run of three wins. Wider columns win sooner and narrower ones later, and this
/// is the one ratio for all of them because a threshold per width is a table that has to be right
/// nine times rather than once. It is a constant with a name so that the sweep that eventually moves
/// it has something to move.
const RUNS_PAY_AT: usize = 2;

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
const NOWHERE: usize = usize::MAX;

/// A run of data copied at the given positions, with a zero wherever the position is [`NOWHERE`].
///
/// A zero and not a skip, because every layout here is a parallel array to a validity mask and a
/// short one would put every value after the first null at the wrong index. It is the same rule
/// [`push_value`] follows for a null.
fn copy_of(data: &Data, at: &[usize]) -> Data {
    macro_rules! copied {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                Data::Empty => Data::Empty,
                $(Data::$variant(values) => {
                    let mut out = Buffer::with_capacity(at.len());
                    for &index in at {
                        // One bounds check rather than a null test and a bounds check, because
                        // `NOWHERE` is past the end of every slice there can be.
                        out.push(values.get(index).copied().unwrap_or($zero));
                    }
                    Data::$variant(out)
                })+
                // The one layout where a gather is a copy of bytes rather than a copy of fixed
                // width slots, and the reason compaction is a decision rather than a default on a
                // string column.
                Data::Varlen(values) => {
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
fn layout_of(data: &Data) -> rudb_common::PhysicalType {
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
        LogicalType::Varchar => data.str_at(index).map(|s| Value::Varchar(s.to_string())),
        LogicalType::Blob | LogicalType::Bit => {
            data.bytes_at(index).map(|bytes| Value::Blob(bytes.to_vec()))
        }
        LogicalType::Date => signed().and_then(|x| i32::try_from(x).ok()).map(Value::Date),
        LogicalType::Time | LogicalType::TimeTz => {
            signed().and_then(|x| i64::try_from(x).ok()).map(Value::Time)
        }
        LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs
        | LogicalType::TimestampTz => {
            signed().and_then(|x| i64::try_from(x).ok()).map(Value::Timestamp)
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

/// An empty run of data of the right layout for a type.
fn empty_data_for(ty: &LogicalType) -> Result<Data> {
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
            Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => v.push(*x),
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

    use rudb_common::{LogicalType, Value};

    use super::{Body, Data, Form, VECTOR_SIZE, Vector};
    use crate::string::StringColumn;
    use crate::validity::Validity;

    fn integers(values: &[i32]) -> Vector {
        Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into())).unwrap()
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
        let Body::Dictionary { codes, values: cut } = &piece.body else {
            panic!("a slice of a dictionary is a dictionary");
        };
        assert!(Arc::ptr_eq(whole, cut), "the cut copied the dictionary");
        assert_eq!(codes, &[1, 1, 0], "the codes are the part that is cut");

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

    #[test]
    fn a_slice_past_the_end_is_an_error_rather_than_a_short_vector() {
        let error = integers(&[1, 2, 3]).slice(2, 2).unwrap_err();
        assert!(error.to_string().contains("of a vector of 3"), "{error}");
    }

    #[test]
    fn the_vector_size_is_the_one_the_design_is_built_around() {
        // 1024 and not DuckDB's 2048. A validity mask is 16 u64 words and a vector of string views
        // is 16 KiB, both of which are consequences of this number rather than coincidences.
        assert_eq!(VECTOR_SIZE, 1024);
        assert_eq!(VECTOR_SIZE / 64, 16);
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

    #[test]
    fn flattening_a_flat_vector_is_the_same_vector() {
        let vector = integers(&[1, 2, 3]);
        assert_eq!(vector.flatten().unwrap(), vector);
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
}
