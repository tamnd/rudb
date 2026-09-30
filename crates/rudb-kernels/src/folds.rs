//! The folds of two lists of floats into one number: `list_distance`, `list_inner_product`,
//! `list_negative_inner_product`, `list_cosine_similarity`, `list_cosine_distance` and the names
//! the pin keeps for them.
//!
//! Each is a port of the operation of the same name in the pin's `array_kernels.hpp`, run in the
//! element type the binder picked, so a FLOAT call rounds at every step where the pin's does. The
//! pin checks the whole of the left side for a null element before it reads a row, then the whole
//! of the right side, and only then compares each row's two lengths, so a null anywhere in a side
//! is refused even when the row that holds it has a null on the other side. The column loop here
//! keeps that order, and the row loop is only reached for a layout the column loop does not read.

use std::ops::{Add, Mul, Neg, Sub};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Buffer, Data, Validity, Vector};

/// Which fold a name asks for.
#[derive(Clone, Copy)]
enum Fold {
    Distance,
    Inner,
    NegativeInner,
    CosineSimilarity,
    CosineDistance,
}

impl Fold {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "list_distance" | "<->" => Self::Distance,
            "list_inner_product" | "list_dot_product" => Self::Inner,
            "list_negative_inner_product" | "list_negative_dot_product" => Self::NegativeInner,
            "list_cosine_similarity" => Self::CosineSimilarity,
            "list_cosine_distance" | "<=>" => Self::CosineDistance,
            _ => return None,
        })
    }

    /// The answer for two lists of the same length, or `None` for the empty lists the cosine
    /// folds have no answer for.
    fn apply<T: Real>(self, left: &[T], right: &[T]) -> Option<T> {
        let inner = || left.iter().zip(right).fold(T::ZERO, |sum, (&x, &y)| sum + x * y);
        match self {
            Self::Inner => Some(inner()),
            Self::NegativeInner => Some(-inner()),
            Self::Distance => {
                let squared = left.iter().zip(right).fold(T::ZERO, |sum, (&x, &y)| {
                    let diff = x - y;
                    sum + diff * diff
                });
                Some(squared.sqrt())
            }
            Self::CosineSimilarity | Self::CosineDistance if left.is_empty() => None,
            Self::CosineSimilarity | Self::CosineDistance => {
                let (mut dot, mut norm_l, mut norm_r) = (T::ZERO, T::ZERO, T::ZERO);
                for (&x, &y) in left.iter().zip(right) {
                    dot = dot + x * y;
                    norm_l = norm_l + x * x;
                    norm_r = norm_r + y * y;
                }
                let similarity = dot.divide((norm_l * norm_r).sqrt());
                // std::max(-1, std::min(similarity, 1)) with the pin's argument order, which is
                // what makes a NaN come out as -1 there rather than staying a NaN.
                let capped = if T::ONE < similarity { T::ONE } else { similarity };
                let clamped = if -T::ONE < capped { capped } else { -T::ONE };
                Some(match self {
                    Self::CosineDistance => T::ONE - clamped,
                    _ => clamped,
                })
            }
        }
    }
}

/// FLOAT and DOUBLE, the two element types the folds are declared over.
trait Real:
    Copy
    + PartialOrd
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Neg<Output = Self>
{
    const ZERO: Self;
    const ONE: Self;
    const TYPE: LogicalType;
    fn sqrt(self) -> Self;
    fn divide(self, by: Self) -> Self;
    fn slice(data: &Data) -> Option<&[Self]>;
    fn data(values: Vec<Self>) -> Data;
    fn read(value: &Value) -> Option<Self>;
    fn value(self) -> Value;
}

impl Real for f32 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;
    const TYPE: LogicalType = LogicalType::Float;
    fn sqrt(self) -> Self {
        self.sqrt()
    }
    fn divide(self, by: Self) -> Self {
        self / by
    }
    fn slice(data: &Data) -> Option<&[Self]> {
        match data {
            Data::Float32(values) => Some(values.as_slice()),
            _ => None,
        }
    }
    fn data(values: Vec<Self>) -> Data {
        Data::Float32(Buffer::from(values))
    }
    fn read(value: &Value) -> Option<Self> {
        match value {
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }
    fn value(self) -> Value {
        Value::Float(self)
    }
}

impl Real for f64 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;
    const TYPE: LogicalType = LogicalType::Double;
    fn sqrt(self) -> Self {
        self.sqrt()
    }
    fn divide(self, by: Self) -> Self {
        self / by
    }
    fn slice(data: &Data) -> Option<&[Self]> {
        match data {
            Data::Float64(values) => Some(values.as_slice()),
            _ => None,
        }
    }
    fn data(values: Vec<Self>) -> Data {
        Data::Float64(Buffer::from(values))
    }
    fn read(value: &Value) -> Option<Self> {
        match value {
            Value::Double(x) => Some(*x),
            _ => None,
        }
    }
    fn value(self) -> Value {
        Value::Double(self)
    }
}

/// The name the way the pin writes it in these errors, which is `SQLIdentifier`: bare when it is
/// a plain lower case word and in double quotes otherwise, so `<->` is written `"<->"`.
fn identifier(name: &str) -> String {
    let plain = name.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        && !name.starts_with('_');
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

fn has_null(name: &str, side: &str) -> Error {
    Error::invalid_input(format!(
        "{}: {side} argument can not contain NULL values",
        identifier(name)
    ))
}

fn unequal(name: &str, left: usize, right: usize) -> Error {
    Error::invalid_input(format!(
        "{}: list dimensions must be equal, got left length '{left}' and right length '{right}'",
        identifier(name)
    ))
}

/// The answer for one row, with neither list null.
pub(crate) fn value(name: &str, args: &[Value]) -> Option<Result<Value>> {
    let fold = Fold::of(name)?;
    let [Value::List { element, values: left }, Value::List { values: right, .. }] = args else {
        return None;
    };
    Some(match element {
        LogicalType::Float => row::<f32>(name, fold, left, right),
        _ => row::<f64>(name, fold, left, right),
    })
}

fn row<T: Real>(name: &str, fold: Fold, left: &[Value], right: &[Value]) -> Result<Value> {
    let read = |values: &[Value], side| -> Result<Vec<T>> {
        values.iter().map(|value| T::read(value).ok_or_else(|| has_null(name, side))).collect()
    };
    let (left, right) = (read(left, "left")?, read(right, "right")?);
    if left.len() != right.len() {
        return Err(unequal(name, left.len(), right.len()));
    }
    Ok(fold.apply(&left, &right).map_or(Value::Null, T::value))
}

/// One side of a call over a column: a list column laid out as entries over a child, or one
/// list for every row, which is what a list literal is.
enum Side<'a, T> {
    Column { list: &'a Vector, entries: &'a [(u32, u32)], values: &'a [T] },
    Constant(Vec<T>),
}

impl<'a, T: Real> Side<'a, T> {
    /// The side, or `None` for a layout this loop does not read. A null element is refused here,
    /// which is where the pin refuses it, before any row is looked at.
    fn read(name: &str, side: &str, vector: &'a Vector) -> Result<Option<Self>> {
        if let Some(value) = vector.constant_value() {
            return match value {
                Value::List { values, .. } => {
                    let read: Option<Vec<T>> = values.iter().map(T::read).collect();
                    match read {
                        Some(read) => Ok(Some(Self::Constant(read))),
                        None if values.iter().any(Value::is_null) => Err(has_null(name, side)),
                        None => Ok(None),
                    }
                }
                _ => Ok(None),
            };
        }
        let Some((entries, child)) = vector.list_parts() else {
            return Ok(None);
        };
        let Some(values) = child.data().and_then(T::slice) else {
            return Ok(None);
        };
        let (rows, elements) = (vector.validity().live(), child.validity().live());
        for (row, &(start, len)) in entries.iter().enumerate() {
            if rows.at(row) && (start..start + len).any(|at| !elements.at(at as usize)) {
                return Err(has_null(name, side));
            }
        }
        Ok(Some(Self::Column { list: vector, entries, values }))
    }

    fn at(&self, row: usize) -> Option<&[T]> {
        match self {
            Self::Column { list, entries, values } => {
                let (start, len) = entries[row];
                let start = start as usize;
                list.validity().is_valid(row).then(|| &values[start..start + len as usize])
            }
            Self::Constant(held) => Some(held),
        }
    }
}

/// The call over whole vectors, or `None` when either side is laid out in a way the loop does not
/// read, which leaves it to [`value`] a row at a time.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let (Some(fold), [left, right]) = (Fold::of(name), args) else {
        return Ok(None);
    };
    match returns {
        LogicalType::Float => column::<f32, _>(name, fold, left, right, rows),
        LogicalType::Double => column::<f64, _>(name, fold, left, right, rows),
        _ => Ok(None),
    }
}

fn column<T: Real, V: AsRef<Vector>>(
    name: &str,
    fold: Fold,
    left: &V,
    right: &V,
    rows: usize,
) -> Result<Option<Vector>> {
    // A null literal on either side folds the call to a null before it runs on the pin, so the
    // other side is not checked for nulls at all.
    let null = |side: &V| side.as_ref().constant_value().is_some_and(Value::is_null);
    if null(left) || null(right) {
        return Ok(Some(Vector::constant(T::TYPE, Value::Null, rows)));
    }
    let Some(left) = Side::<T>::read(name, "left", left.as_ref())? else {
        return Ok(None);
    };
    let Some(right) = Side::<T>::read(name, "right", right.as_ref())? else {
        return Ok(None);
    };
    let mut answers = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(rows);
    for row in 0..rows {
        let answer = match (left.at(row), right.at(row)) {
            (Some(l), Some(r)) if l.len() != r.len() => {
                return Err(unequal(name, l.len(), r.len()));
            }
            (Some(l), Some(r)) => fold.apply(l, r),
            _ => None,
        };
        valid.push(answer.is_some());
        answers.push(answer.unwrap_or(T::ZERO));
    }
    let validity = Validity::from_iter(rows, |row| valid[row]).normalize(rows);
    Ok(Some(Vector::flat(T::TYPE, T::data(answers))?.with_validity(validity)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folds_answer_what_the_pin_answers() {
        let (a, b) = ([1.0_f64, 2.0, 3.0], [4.0_f64, 5.0, 6.0]);
        assert_eq!(Fold::Distance.apply(&a, &b), Some(5.196_152_422_706_632));
        assert_eq!(Fold::Inner.apply(&a, &b), Some(32.0));
        assert_eq!(Fold::NegativeInner.apply(&a, &b), Some(-32.0));
        assert_eq!(Fold::CosineSimilarity.apply(&a, &b), Some(0.974_631_846_197_076_2));
        assert_eq!(Fold::CosineDistance.apply(&a, &b), Some(0.025_368_153_802_923_787));
        let (a, b) = ([3.0_f32, 4.0], [4.0_f32, 3.0]);
        assert_eq!(Fold::CosineDistance.apply(&a, &b), Some(0.040_000_02));
    }

    #[test]
    fn a_cosine_of_nothing_is_null_and_of_a_nan_is_minus_one() {
        assert_eq!(Fold::CosineSimilarity.apply::<f64>(&[], &[]), None);
        assert_eq!(Fold::Distance.apply::<f64>(&[], &[]), Some(0.0));
        assert_eq!(Fold::CosineSimilarity.apply(&[0.0_f64, 0.0], &[1.0, 2.0]), Some(-1.0));
        assert_eq!(Fold::CosineDistance.apply(&[0.0_f64, 0.0], &[1.0, 2.0]), Some(2.0));
        assert_eq!(Fold::CosineSimilarity.apply(&[1e200_f64, 1e200], &[1e200, 1e200]), Some(-1.0));
    }

    #[test]
    fn an_operator_name_is_quoted_in_the_message() {
        assert_eq!(identifier("list_distance"), "list_distance");
        assert_eq!(identifier("<->"), "\"<->\"");
    }
}
