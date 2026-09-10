//! `AND` and `OR` over three values.
//!
//! The two rules that matter are that `FALSE AND NULL` is false and `TRUE OR NULL` is true. Both
//! are the reason a conjunction cannot be evaluated by treating null as false and then fixing it up
//! afterwards: `NULL AND FALSE` is false, `NULL AND TRUE` is null, and no single substitution for
//! null gets both.
//!
//! A conjunction here is flat, over two or more children, because that is the shape the binder
//! produces and the shape filter pushdown wants. Evaluating a flat one is a fold with an early
//! answer, which is also why the null case is cheap: once a false has been seen in an `AND` nothing
//! any other child says can change the result.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::Vector;

/// Which connective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Connective {
    /// `AND`.
    And,
    /// `OR`.
    Or,
}

/// Combines two or more boolean vectors.
///
/// # Errors
///
/// If there are no children, if they are not all the same length, or if one of them is not boolean.
pub fn combine(op: Connective, children: &[Vector]) -> Result<Vector> {
    let first =
        children.first().ok_or_else(|| Error::internal("a conjunction with no children"))?;
    let rows = first.len();
    for (at, child) in children.iter().enumerate() {
        if child.len() != rows {
            return Err(Error::internal(format!(
                "child {at} of a conjunction is {} rows and child 0 is {rows}",
                child.len()
            )));
        }
    }
    let mut values = Vec::with_capacity(rows);
    for index in 0..rows {
        let mut answer = Some(matches!(op, Connective::And));
        for child in children {
            let held = match child.value_at(index) {
                Value::Boolean(held) => Some(held),
                Value::Null => None,
                other => {
                    return Err(Error::internal(format!(
                        "a conjunction over a {} value",
                        other.logical_type()
                    )));
                }
            };
            answer = fold(op, answer, held);
        }
        values.push(match answer {
            Some(held) => Value::Boolean(held),
            None => Value::Null,
        });
    }
    Vector::from_values(LogicalType::Boolean, &values)
}

/// One step of the fold, where `None` is unknown.
///
/// The short circuit is on the value rather than on the position: a false anywhere in an `AND`
/// wins over an unknown that came before it, which is exactly the case a two-valued fold gets
/// wrong.
fn fold(op: Connective, left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match op {
        Connective::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Connective::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
    }
}

/// Whether a predicate keeps a row.
///
/// True keeps it, and false and null both drop it. That is `WHERE`'s rule and it is not `CHECK`'s,
/// which keeps a row whose constraint is unknown.
#[must_use]
pub fn is_true(value: &Value) -> bool {
    matches!(value, Value::Boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(values: &[Value]) -> Vector {
        Vector::from_values(LogicalType::Boolean, values).expect("booleans")
    }

    const TRUE: Value = Value::Boolean(true);
    const FALSE: Value = Value::Boolean(false);

    #[test]
    fn a_false_wins_an_and_even_against_an_unknown() {
        let result = combine(Connective::And, &[vector(&[Value::Null]), vector(&[FALSE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), FALSE);
    }

    #[test]
    fn a_true_wins_an_or_even_against_an_unknown() {
        let result = combine(Connective::Or, &[vector(&[Value::Null]), vector(&[TRUE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), TRUE);
    }

    #[test]
    fn an_unknown_survives_when_nothing_decides_it() {
        let result = combine(Connective::And, &[vector(&[Value::Null]), vector(&[TRUE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), Value::Null);
        let result = combine(Connective::Or, &[vector(&[Value::Null]), vector(&[FALSE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), Value::Null);
    }

    #[test]
    fn a_flat_conjunction_of_more_than_two_children_is_one_pass() {
        let result = combine(
            Connective::And,
            &[vector(&[TRUE]), vector(&[TRUE]), vector(&[TRUE]), vector(&[FALSE])],
        )
        .expect("four booleans");
        assert_eq!(result.value_at(0), FALSE);
    }

    #[test]
    fn a_where_clause_drops_the_rows_it_cannot_decide() {
        assert!(is_true(&TRUE));
        assert!(!is_true(&FALSE));
        assert!(!is_true(&Value::Null));
    }

    #[test]
    fn a_conjunction_with_no_children_is_caught() {
        let error = combine(Connective::And, &[]).expect_err("nothing to combine");
        assert!(error.message().contains("no children"), "{error}");
    }
}
