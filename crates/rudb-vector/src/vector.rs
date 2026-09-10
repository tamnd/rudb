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

use rudb_common::{Error, LogicalType, Result, Value};

use crate::string::StringColumn;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    Bool(Vec<bool>),
    /// 8 bit signed.
    Int8(Vec<i8>),
    /// 16 bit signed.
    Int16(Vec<i16>),
    /// 32 bit signed.
    Int32(Vec<i32>),
    /// 64 bit signed.
    Int64(Vec<i64>),
    /// 128 bit signed.
    Int128(Vec<i128>),
    /// 8 bit unsigned.
    UInt8(Vec<u8>),
    /// 16 bit unsigned.
    UInt16(Vec<u16>),
    /// 32 bit unsigned.
    UInt32(Vec<u32>),
    /// 64 bit unsigned.
    UInt64(Vec<u64>),
    /// 128 bit unsigned.
    UInt128(Vec<u128>),
    /// IEEE 754 binary32.
    Float32(Vec<f32>),
    /// IEEE 754 binary64.
    Float64(Vec<f64>),
    /// The months, days and microseconds triple.
    Interval(Vec<(i32, i32, i64)>),
    /// Strings, as 16 byte views plus the blocks the long ones live in.
    Varlen(StringColumn),
}

impl Data {
    /// How many values are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Bool(v) => v.len(),
            Self::Int8(v) => v.len(),
            Self::Int16(v) => v.len(),
            Self::Int32(v) => v.len(),
            Self::Int64(v) => v.len(),
            Self::Int128(v) => v.len(),
            Self::UInt8(v) => v.len(),
            Self::UInt16(v) => v.len(),
            Self::UInt32(v) => v.len(),
            Self::UInt64(v) => v.len(),
            Self::UInt128(v) => v.len(),
            Self::Float32(v) => v.len(),
            Self::Float64(v) => v.len(),
            Self::Interval(v) => v.len(),
            Self::Varlen(v) => v.len(),
        }
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
        match self {
            Self::Int8(v) => v.get(index).map(|&x| i128::from(x)),
            Self::Int16(v) => v.get(index).map(|&x| i128::from(x)),
            Self::Int32(v) => v.get(index).map(|&x| i128::from(x)),
            Self::Int64(v) => v.get(index).map(|&x| i128::from(x)),
            Self::Int128(v) => v.get(index).copied(),
            _ => None,
        }
    }

    /// An unsigned integer at `index`, widened.
    #[must_use]
    pub fn unsigned_at(&self, index: usize) -> Option<u128> {
        match self {
            Self::UInt8(v) => v.get(index).map(|&x| u128::from(x)),
            Self::UInt16(v) => v.get(index).map(|&x| u128::from(x)),
            Self::UInt32(v) => v.get(index).map(|&x| u128::from(x)),
            Self::UInt64(v) => v.get(index).map(|&x| u128::from(x)),
            Self::UInt128(v) => v.get(index).copied(),
            _ => None,
        }
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
    Sequence { start: i64, step: i64 },
    Dictionary { codes: Vec<u32>, values: Box<Vector> },
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
        Ok(Self {
            ty: values.ty.clone(),
            len: codes.len(),
            validity: Validity::AllValid,
            body: Body::Dictionary { codes, values: Box::new(values) },
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
        let values: Vec<Value> = self.iter().collect();
        let mut data = empty_data_for(&self.ty)?;
        for value in &values {
            push_value(&mut data, value)?;
        }
        // Taken from the values rather than from `self.validity`, because a dictionary keeps its
        // nulls in the vector it points at and its own validity says nothing about them. Reading it
        // instead of them is how a null survives being selected and then comes out as a zero.
        let validity = Validity::from_iter(self.len, |index| !values[index].is_null());
        Ok(Self { ty: self.ty.clone(), len: self.len, validity, body: Body::Flat(data) })
    }
}

/// The physical layout a run of data is in, for the check that it matches its type.
fn layout_of(data: &Data) -> rudb_common::PhysicalType {
    use rudb_common::PhysicalType as P;
    match data {
        Data::Empty => P::Empty,
        Data::Bool(_) => P::Bool,
        Data::Int8(_) => P::Int8,
        Data::Int16(_) => P::Int16,
        Data::Int32(_) => P::Int32,
        Data::Int64(_) => P::Int64,
        Data::Int128(_) => P::Int128,
        Data::UInt8(_) => P::UInt8,
        Data::UInt16(_) => P::UInt16,
        Data::UInt32(_) => P::UInt32,
        Data::UInt64(_) => P::UInt64,
        Data::UInt128(_) => P::UInt128,
        Data::Float32(_) => P::Float32,
        Data::Float64(_) => P::Float64,
        Data::Interval(_) => P::Interval,
        Data::Varlen(_) => P::Varlen,
    }
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
    Ok(match ty.physical() {
        P::Empty => Data::Empty,
        P::Bool => Data::Bool(Vec::new()),
        P::Int8 => Data::Int8(Vec::new()),
        P::Int16 => Data::Int16(Vec::new()),
        P::Int32 => Data::Int32(Vec::new()),
        P::Int64 => Data::Int64(Vec::new()),
        P::Int128 => Data::Int128(Vec::new()),
        P::UInt8 => Data::UInt8(Vec::new()),
        P::UInt16 => Data::UInt16(Vec::new()),
        P::UInt32 => Data::UInt32(Vec::new()),
        P::UInt64 => Data::UInt64(Vec::new()),
        P::UInt128 => Data::UInt128(Vec::new()),
        P::Float32 => Data::Float32(Vec::new()),
        P::Float64 => Data::Float64(Vec::new()),
        P::Interval => Data::Interval(Vec::new()),
        P::Varlen => Data::Varlen(StringColumn::new()),
        other => {
            return Err(Error::not_implemented(format!(
                "a flat vector of {other:?} data, which arrives with the storage layer"
            )));
        }
    })
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
    match data {
        Data::Empty => {}
        Data::Bool(v) => push!(v, Value::Boolean, false),
        Data::Int8(v) => push!(v, Value::TinyInt, 0),
        Data::Int16(v) => push!(v, Value::SmallInt, 0),
        Data::Int32(v) => match value {
            Value::Null => v.push(0),
            Value::Integer(x) | Value::Date(x) => v.push(*x),
            other => return Err(Error::internal(format!("{other:?} is not a 32 bit value"))),
        },
        Data::Int64(v) => match value {
            Value::Null => v.push(0),
            Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => v.push(*x),
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
    use rudb_common::{LogicalType, Value};

    use super::{Data, Form, VECTOR_SIZE, Vector};
    use crate::string::StringColumn;
    use crate::validity::Validity;

    fn integers(values: &[i32]) -> Vector {
        Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec())).unwrap()
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
        let wrong = Vector::flat(LogicalType::Varchar, Data::Int32(vec![1]));
        assert!(wrong.is_err());
        let right = Vector::flat(LogicalType::Date, Data::Int32(vec![1]));
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

    #[test]
    fn flattening_a_flat_vector_is_the_same_vector() {
        let vector = integers(&[1, 2, 3]);
        assert_eq!(vector.flatten().unwrap(), vector);
    }

    #[test]
    fn a_decimal_reads_its_width_and_scale_from_the_type_and_not_the_data() {
        let ty = LogicalType::decimal(9, 2).unwrap();
        let vector = Vector::flat(ty, Data::Int32(vec![1234])).unwrap();
        assert_eq!(vector.value_at(0), Value::Decimal { unscaled: 1234, width: 9, scale: 2 });
        assert_eq!(vector.value_at(0).to_string(), "12.34");
    }
}
