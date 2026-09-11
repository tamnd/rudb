//! The vector itself.
//!
//! `spec/07-execution.md` section 7.1 calls this the widest interface in the system, says every
//! operator depends on it, and says changing it after twenty operators exist is expensive. So it
//! is written before the first operator rather than after the fifth.
//!
//! A vector is a type, a length of at most [`VECTOR_SIZE`], a physical form, a validity
//! representation and some data. The four forms are the ones in `spec/04-architecture.md` section
//! 4.3: flat, constant, sequence and dictionary. Encoded, the fifth, is the M3 work and it arrives
//! with the specialization contract rather than before it.
//!
//! **What is not here yet.** Buffers are owned. Section 7.1 says a vector borrowed from a buffer
//! managed page carries a pin, and there is no buffer manager until M2, so there is nothing to pin
//! and pretending otherwise would be an interface built against an imaginary caller. Nested types
//! are not stored yet either, for the same reason: a `LIST(STRUCT(...))` is offsets plus child
//! column chunks, and child column chunks are storage.

use std::sync::Arc;

use rudb_common::{Error, LogicalType, Result, Value};

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
            Body::Flat(data) => value_from(&self.ty, data, index),
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
    /// # Errors
    ///
    /// If the type is one this crate cannot store flat yet, which today means the nested types.
    pub fn flatten(&self) -> Result<Self> {
        if let Body::Flat(_) = self.body {
            return Ok(self.clone());
        }
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
            // Unreachable, because `resolve` stops at the first body that is not a dictionary.
            Body::Dictionary { .. } => {
                return Err(Error::internal("a dictionary survived being resolved"));
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
            let Body::Dictionary { codes, values } = &source.body else {
                return (at, source);
            };
            for slot in &mut at {
                *slot = match codes.get(*slot) {
                    Some(&code) => code as usize,
                    None => NOWHERE,
                };
            }
            source = values.as_ref();
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
            data.str_at(index).map(|s| Value::Blob(s.as_bytes().to_vec()))
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
            // A blob is bytes and this column is text, so the only blobs that survive a round trip
            // here are the ones that happen to be valid UTF-8. Real blob storage is a byte column
            // and it arrives with the storage layer at M2 rather than being faked now.
            Value::Blob(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => {
                    column.push(text);
                }
                Err(_) => {
                    return Err(Error::not_implemented(
                        "a blob that is not valid UTF-8, which needs the byte column from M2",
                    ));
                }
            },
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

    #[test]
    fn a_decimal_too_wide_for_the_run_its_type_chose_is_an_error_and_not_a_wrong_number() {
        // Only reachable by hand, since a value's width is what picked the run. Truncating here
        // would store a different number and say nothing about it.
        let ty = LogicalType::decimal(4, 1).unwrap();
        let value = Value::Decimal { unscaled: 1_000_000, width: 4, scale: 1 };
        let error = Vector::from_values(ty, &[value]).unwrap_err();
        assert!(error.to_string().contains("does not fit"), "{error}");
    }
}
