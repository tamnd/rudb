//! Comparing values, which is where SQL's three-valued logic actually lives.
//!
//! Six of the eight comparisons return null when either side is null, and the other two never do.
//! That is not a detail: `WHERE a = b` drops a row where either is null and `WHERE a IS NOT
//! DISTINCT FROM b` keeps the row where both are, and the binder produces the second one for `IS
//! NULL` and for a `USING` join under some rewrites. One enum with the null rule attached to the
//! variant is what stops that difference from being re-decided in every operator.
//!
//! The comparison enum here is this crate's own rather than `rudb_plan`'s, because the plan sits
//! nine ranks above the kernels and a kernel that imports a plan type is a kernel that cannot be
//! called from anywhere else. The executor maps one to the other, which is four lines it writes
//! once.
//!
//! Float comparison is DuckDB's rather than IEEE's. Two NaNs are equal, NaN sorts above every
//! number, and negative zero equals zero. IEEE says the first is false and that a NaN comparison is
//! unordered, which would make `GROUP BY` over a column with a NaN in it produce a group nothing
//! can ever find again and make a sort's result depend on the order the rows arrived in.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Form, Vector};

use crate::number::{approximate, integral};

/// Which comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Comparison {
    /// `=`, null if either side is null.
    Equal,
    /// `<>`, null if either side is null.
    NotEqual,
    /// `<`, null if either side is null.
    Less,
    /// `<=`, null if either side is null.
    LessOrEqual,
    /// `>`, null if either side is null.
    Greater,
    /// `>=`, null if either side is null.
    GreaterOrEqual,
    /// `IS DISTINCT FROM`, which is total and never null.
    DistinctFrom,
    /// `IS NOT DISTINCT FROM`, which is total and never null.
    NotDistinctFrom,
}

impl Comparison {
    /// Whether this comparison treats null as a value rather than as an absence.
    #[must_use]
    pub fn is_total(self) -> bool {
        matches!(self, Self::DistinctFrom | Self::NotDistinctFrom)
    }
}

/// Compares two vectors of the same length, producing a `BOOLEAN` vector.
///
/// # Errors
///
/// If the two sides are not the same length, or if the two types cannot be compared.
pub fn compare(op: Comparison, left: &Vector, right: &Vector) -> Result<Vector> {
    if left.len() != right.len() {
        return Err(Error::internal(format!(
            "a comparison of a {} row vector with a {} row one",
            left.len(),
            right.len()
        )));
    }
    if left.form() == Form::Constant && right.form() == Form::Constant && !left.is_empty() {
        let single = compare_values(op, &left.value_at(0), &right.value_at(0))?;
        return Ok(Vector::constant(LogicalType::Boolean, single, left.len()));
    }
    let mut values = Vec::with_capacity(left.len());
    for index in 0..left.len() {
        values.push(compare_values(op, &left.value_at(index), &right.value_at(index))?);
    }
    Vector::from_values(LogicalType::Boolean, &values)
}

/// Compares two values, producing `TRUE`, `FALSE` or `NULL`.
///
/// # Errors
///
/// If the two types cannot be compared, which after binding means one of them is a nested type.
pub fn compare_values(op: Comparison, left: &Value, right: &Value) -> Result<Value> {
    if op.is_total() {
        let same = match (left.is_null(), right.is_null()) {
            (true, true) => true,
            (true, false) | (false, true) => false,
            (false, false) => order(left, right)? == Ordering::Equal,
        };
        return Ok(Value::Boolean(match op {
            Comparison::NotDistinctFrom => same,
            _ => !same,
        }));
    }
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let ordering = order(left, right)?;
    let held = match op {
        Comparison::Equal => ordering == Ordering::Equal,
        Comparison::NotEqual => ordering != Ordering::Equal,
        Comparison::Less => ordering == Ordering::Less,
        Comparison::LessOrEqual => ordering != Ordering::Greater,
        Comparison::Greater => ordering == Ordering::Greater,
        Comparison::GreaterOrEqual => ordering != Ordering::Less,
        Comparison::DistinctFrom | Comparison::NotDistinctFrom => {
            return Err(Error::internal("a total comparison reached the ordered path"));
        }
    };
    Ok(Value::Boolean(held))
}

/// The order of two values, neither of which is null.
///
/// This is the one place the sort order of a type is written down. `ORDER BY`, `GROUP BY`, a merge
/// join and a min or max aggregate all reach it, and a type that ordered differently in two of
/// those would produce a query whose answer depends on which operator the optimizer picked.
///
/// # Errors
///
/// If either value is null, which is the caller's mistake rather than a comparison, or if the
/// types have no order between them.
pub fn order(left: &Value, right: &Value) -> Result<Ordering> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => {
            Err(Error::internal("a null reached the ordering path"))
        }
        (Value::Boolean(a), Value::Boolean(b)) => Ok(a.cmp(b)),
        (Value::Varchar(a), Value::Varchar(b)) => Ok(a.as_bytes().cmp(b.as_bytes())),
        (Value::Blob(a), Value::Blob(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) | (Value::Timestamp(a), Value::Timestamp(b)) => {
            Ok(a.cmp(b))
        }
        (
            Value::Interval { months: am, days: ad, micros: au },
            Value::Interval { months: bm, days: bd, micros: bu },
        ) => Ok((am, ad, au).cmp(&(bm, bd, bu))),
        _ => numeric_order(left, right),
    }
}

/// The order of two numbers, which is the case that has to work across representations.
fn numeric_order(left: &Value, right: &Value) -> Result<Ordering> {
    if let (Some(a), Some(b)) = (integral(left), integral(right)) {
        return Ok(a.cmp(&b));
    }
    if let (
        Value::Decimal { unscaled: a, scale: sa, .. },
        Value::Decimal { unscaled: b, scale: sb, .. },
    ) = (left, right)
    {
        if sa == sb {
            return Ok(a.cmp(b));
        }
    }
    match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => Ok(float_order(a, b)),
        _ => Err(Error::not_implemented(format!(
            "comparing {} with {}",
            left.logical_type(),
            right.logical_type()
        ))),
    }
}

/// DuckDB's float order: NaN is equal to itself and above everything else, and zero has one place.
fn float_order(left: f64, right: f64) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

/// The order of two values with nulls in it, for a sort key.
///
/// A sort has to put nulls somewhere and SQL lets the query say where, so this takes the answer
/// rather than deciding it.
///
/// # Errors
///
/// If the two types have no order between them.
pub fn order_with_nulls(left: &Value, right: &Value, nulls_first: bool) -> Result<Ordering> {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => order(left, right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compared(op: Comparison, left: Value, right: Value) -> Value {
        compare_values(op, &left, &right).expect("these types compare")
    }

    #[test]
    fn an_ordinary_comparison_is_null_when_either_side_is() {
        assert_eq!(compared(Comparison::Equal, Value::Integer(1), Value::Null), Value::Null);
        assert_eq!(compared(Comparison::Less, Value::Null, Value::Integer(1)), Value::Null);
    }

    #[test]
    fn a_total_comparison_is_never_null() {
        assert_eq!(
            compared(Comparison::NotDistinctFrom, Value::Null, Value::Null),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::NotDistinctFrom, Value::Integer(1), Value::Null),
            Value::Boolean(false)
        );
        assert_eq!(
            compared(Comparison::DistinctFrom, Value::Integer(1), Value::Null),
            Value::Boolean(true)
        );
    }

    #[test]
    fn a_string_compares_by_bytes() {
        assert_eq!(
            compared(Comparison::Less, Value::Varchar("a".into()), Value::Varchar("b".into())),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Less, Value::Varchar("Z".into()), Value::Varchar("a".into())),
            Value::Boolean(true)
        );
    }

    /// The reason this crate does not use `f64::partial_cmp` directly. A NaN that compared
    /// unordered would make a group by produce a group nothing can find again.
    #[test]
    fn two_nans_are_one_value_and_they_sort_above_the_numbers() {
        assert_eq!(
            compared(Comparison::Equal, Value::Double(f64::NAN), Value::Double(f64::NAN)),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Greater, Value::Double(f64::NAN), Value::Double(1e300)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn zero_has_one_value_however_it_is_signed() {
        assert_eq!(
            compared(Comparison::Equal, Value::Double(0.0), Value::Double(-0.0)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn a_number_compares_the_same_however_it_is_stored() {
        assert_eq!(
            compared(Comparison::Equal, Value::Integer(3), Value::BigInt(3)),
            Value::Boolean(true)
        );
        assert_eq!(
            compared(Comparison::Less, Value::Integer(3), Value::Double(3.5)),
            Value::Boolean(true)
        );
    }

    #[test]
    fn nulls_go_where_the_query_asked_for_them() {
        assert_eq!(
            order_with_nulls(&Value::Null, &Value::Integer(1), true).expect("orders"),
            Ordering::Less
        );
        assert_eq!(
            order_with_nulls(&Value::Null, &Value::Integer(1), false).expect("orders"),
            Ordering::Greater
        );
    }

    #[test]
    fn two_constant_vectors_cost_one_comparison() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 512);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(2), 512);
        let result = compare(Comparison::Less, &left, &right).expect("compares");
        assert_eq!(result.form(), Form::Constant);
        assert_eq!(result.value_at(500), Value::Boolean(true));
    }

    #[test]
    fn a_comparison_of_two_vectors_is_one_answer_per_row() {
        let left = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(5), Value::Null],
        )
        .expect("three rows");
        let right = Vector::constant(LogicalType::Integer, Value::Integer(3), 3);
        let result = compare(Comparison::Greater, &left, &right).expect("compares");
        assert_eq!(result.value_at(0), Value::Boolean(false));
        assert_eq!(result.value_at(1), Value::Boolean(true));
        assert_eq!(result.value_at(2), Value::Null);
    }

    #[test]
    fn two_vectors_of_different_lengths_are_caught() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 4);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 5);
        let error = compare(Comparison::Equal, &left, &right).expect_err("ragged");
        assert!(error.message().contains("4 row vector"), "{error}");
    }
}
