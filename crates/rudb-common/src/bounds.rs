//! What a minimum and a maximum can answer about a comparison, without looking at any rows.
//!
//! Every part of this engine that stores data stores the smallest and the largest value of a stretch
//! of it, and every one of them then wants to ask the same question: given `x < 5`, can this stretch
//! hold a row that passes. The stretches differ. A Parquet row group keeps its bounds in the footer
//! as the writer's bytes. A table in memory keeps them per chunk as values. A block of the storage
//! format will keep them per block. What does not differ is the answer, so the answer lives here and
//! the three of them decode into [`Bound`] and call [`excluded`].
//!
//! That is worth a module at rank zero rather than a function each. Three copies of this reasoning
//! would be three chances to get the direction of one comparison backwards, and the failure that
//! causes is not a slow query: it is a row group dropped from an answer that needed it. A wrong
//! answer, arrived at quickly.
//!
//! # Every answer here is one sided
//!
//! [`excluded`] says a stretch cannot hold a matching row, or it says nothing. It never says a
//! stretch does hold one, because bounds cannot know that: a column running from 1 to 100 may hold
//! no 50 at all. So the caller skips on `true` and reads on `false`, and every case this cannot
//! decide answers `false`, which costs time and never costs rows.
//!
//! # What has no bound
//!
//! A null answers `None` from [`Bound::of_value`]. A comparison against null is null, so a filter
//! holding one keeps no rows anywhere, and that is a fact about the whole query rather than about
//! one stretch of it. Deciding it here would be deciding it in the wrong place.
//!
//! A `NaN` compares with nothing, which falls out of `f64::partial_cmp`. A column whose minimum is
//! `NaN` has no minimum, so no test against it rules anything out, and the `None` from the ordering
//! carries that through without a case of its own.
//!
//! Times and timestamps are a gap rather than a decision. A [`Value::Timestamp`] is microseconds and
//! a file is free to store the same column in milliseconds or nanoseconds, so comparing the two
//! would rule out stretches holding rows the query wants. Closing that means carrying the unit
//! alongside the bound, which is a change to make when a benchmark asks for it.

use std::cmp::Ordering;

use crate::Value;

/// The comparison a bounds test applies.
///
/// The five that a minimum and a maximum can answer, written with the column on the left. `<>` is
/// not here: a stretch is ruled out for it only when its bounds are equal to each other and to the
/// constant, which is a column of one value and not worth a case. The two distinctness operators are
/// about nulls rather than about order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `=`.
    Equal,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
}

impl Op {
    /// The same comparison with its operands the other way round, for `5 < x` written as such.
    ///
    /// The optimizer does not normalise which side of a comparison the constant sits on, so a caller
    /// turning a filter into a test flips the operator when it finds the column on the right.
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Equal => Self::Equal,
            Self::Less => Self::Greater,
            Self::LessOrEqual => Self::GreaterOrEqual,
            Self::Greater => Self::Less,
            Self::GreaterOrEqual => Self::LessOrEqual,
        }
    }
}

/// A constant, in a domain that can be ordered against a stored bound.
///
/// Three domains and not one per type, because a bound written for a `SMALLINT` has to compare with
/// a constant the parser read as an `INTEGER`, and widening both to the same domain is what makes
/// that one comparison instead of a table of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Bound {
    /// Every integer, boolean and date, widened.
    Int(i128),
    /// `FLOAT` and `DOUBLE`.
    Real(f64),
    /// `VARCHAR` and `BLOB`, ordered as bytes.
    Bytes(Vec<u8>),
}

impl Bound {
    /// The bound a value stands for, or `None` for a value no bound compares with.
    ///
    /// See the module doc for which values answer `None` and why.
    #[must_use]
    pub fn of_value(value: &Value) -> Option<Self> {
        Some(match value {
            Value::Boolean(flag) => Self::Int(i128::from(*flag)),
            Value::TinyInt(number) => Self::Int(i128::from(*number)),
            Value::SmallInt(number) => Self::Int(i128::from(*number)),
            Value::Integer(number) => Self::Int(i128::from(*number)),
            Value::BigInt(number) => Self::Int(i128::from(*number)),
            Value::HugeInt(number) => Self::Int(*number),
            Value::UTinyInt(number) => Self::Int(i128::from(*number)),
            Value::USmallInt(number) => Self::Int(i128::from(*number)),
            Value::UInteger(number) => Self::Int(i128::from(*number)),
            Value::UBigInt(number) => Self::Int(i128::from(*number)),
            Value::UHugeInt(number) => Self::Int(i128::try_from(*number).ok()?),
            Value::Date(days) => Self::Int(i128::from(*days)),
            Value::Float(number) => Self::Real(f64::from(*number)),
            Value::Double(number) => Self::Real(*number),
            Value::Varchar(text) => Self::Bytes(text.as_bytes().to_vec()),
            Value::Blob(bytes) => Self::Bytes(bytes.clone()),
            _ => return None,
        })
    }

    /// The order between two bounds of the same domain, and `None` across domains.
    #[must_use]
    pub fn order(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => Some(left.cmp(right)),
            (Self::Real(left), Self::Real(right)) => left.partial_cmp(right),
            (Self::Bytes(left), Self::Bytes(right)) => Some(left.as_slice().cmp(right)),
            _ => None,
        }
    }

    /// This bound if it is smaller than `other`, which is how a minimum is accumulated.
    #[must_use]
    pub fn smaller(self, other: Self) -> Self {
        match self.order(&other) {
            Some(Ordering::Greater) => other,
            _ => self,
        }
    }

    /// This bound if it is larger than `other`, which is how a maximum is accumulated.
    #[must_use]
    pub fn larger(self, other: Self) -> Self {
        match self.order(&other) {
            Some(Ordering::Less) => other,
            _ => self,
        }
    }
}

/// Whether `column op value` is false for every value between `low` and `high`.
///
/// `low` and `high` are the smallest and the largest value of the stretch being tested, either of
/// which may be missing, because a writer is allowed to record one and not the other and a column
/// that is entirely null has neither.
///
/// Which end each comparison needs is the whole of it. `x < c` is false everywhere only when even
/// the smallest value in the stretch is not below `c`, and the mirror holds for the other three.
/// `x = c` needs both ends, because the constant has to fall outside the range on one side or the
/// other.
///
/// Every arm is written as a comparison that has to come back known and in a stated direction, so a
/// comparison that cannot be made at all keeps the stretch. Writing it the other way round, as the
/// negation of the comparison that keeps rows, reads the same and is not the same: a `NaN` bound or
/// a constant from another domain answers neither direction, and negating that turns an unknown into
/// a skip and drops rows from an answer.
#[must_use]
pub fn excluded(op: Op, value: &Bound, low: Option<&Bound>, high: Option<&Bound>) -> bool {
    match op {
        // Nothing is below `c` when the smallest value is already `c` or more.
        Op::Less => holds(low, value, &[Ordering::Greater, Ordering::Equal]),
        // Nothing is at or below `c` when the smallest value is above it.
        Op::LessOrEqual => holds(low, value, &[Ordering::Greater]),
        // Nothing is above `c` when the largest value is already `c` or less.
        Op::Greater => holds(high, value, &[Ordering::Less, Ordering::Equal]),
        // Nothing is at or above `c` when the largest value is below it.
        Op::GreaterOrEqual => holds(high, value, &[Ordering::Less]),
        // `c` has to fall off one end or the other.
        Op::Equal => {
            holds(low, value, &[Ordering::Greater]) || holds(high, value, &[Ordering::Less])
        }
    }
}

/// Whether `bound` is present and stands in one of `wanted` to `value`.
///
/// Absent, or ordered against `value` in no direction at all, answers `false`, which is the answer
/// that keeps the rows.
fn holds(bound: Option<&Bound>, value: &Bound, wanted: &[Ordering]) -> bool {
    bound.and_then(|bound| bound.order(value)).is_some_and(|order| wanted.contains(&order))
}

#[cfg(test)]
mod tests {
    use super::{Bound, Op, excluded};
    use crate::Value;

    /// The range 10 to 20, which every test here asks about.
    fn range() -> (Bound, Bound) {
        (Bound::Int(10), Bound::Int(20))
    }

    #[test]
    fn a_constant_below_the_range_rules_out_equality_and_nothing_else() {
        let (low, high) = range();
        let five = Bound::Int(5);
        assert!(excluded(Op::Equal, &five, Some(&low), Some(&high)));
        assert!(excluded(Op::Less, &five, Some(&low), Some(&high)), "nothing is below 5");
        assert!(excluded(Op::LessOrEqual, &five, Some(&low), Some(&high)));
        assert!(!excluded(Op::Greater, &five, Some(&low), Some(&high)), "everything is above 5");
        assert!(!excluded(Op::GreaterOrEqual, &five, Some(&low), Some(&high)));
    }

    #[test]
    fn a_constant_above_the_range_rules_out_the_other_direction() {
        let (low, high) = range();
        let fifty = Bound::Int(50);
        assert!(excluded(Op::Equal, &fifty, Some(&low), Some(&high)));
        assert!(!excluded(Op::Less, &fifty, Some(&low), Some(&high)));
        assert!(excluded(Op::Greater, &fifty, Some(&low), Some(&high)));
        assert!(excluded(Op::GreaterOrEqual, &fifty, Some(&low), Some(&high)));
    }

    /// The edges, where an off by one is a wrong answer rather than a slow query.
    #[test]
    fn a_constant_at_either_end_of_the_range_is_kept() {
        let (low, high) = range();
        for value in [Bound::Int(10), Bound::Int(20)] {
            assert!(!excluded(Op::Equal, &value, Some(&low), Some(&high)));
            assert!(!excluded(Op::LessOrEqual, &value, Some(&low), Some(&high)));
            assert!(!excluded(Op::GreaterOrEqual, &value, Some(&low), Some(&high)));
        }
        // `x < 10` is false everywhere in a stretch whose smallest value is 10, and `x > 20` is
        // false everywhere in one whose largest is 20.
        assert!(excluded(Op::Less, &Bound::Int(10), Some(&low), Some(&high)));
        assert!(excluded(Op::Greater, &Bound::Int(20), Some(&low), Some(&high)));
    }

    #[test]
    fn a_missing_bound_rules_nothing_out() {
        let high = Bound::Int(20);
        assert!(!excluded(Op::Less, &Bound::Int(5), None, Some(&high)));
        assert!(!excluded(Op::Equal, &Bound::Int(5), None, None));
    }

    #[test]
    fn a_bound_of_another_domain_rules_nothing_out() {
        let (low, high) = range();
        let text = Bound::Bytes(b"x".to_vec());
        assert!(!excluded(Op::Equal, &text, Some(&low), Some(&high)));
        assert!(!excluded(Op::Less, &text, Some(&low), Some(&high)));
    }

    /// A column of `NaN` has no minimum, so nothing can be ruled out against it.
    #[test]
    fn a_nan_bound_rules_nothing_out() {
        let nan = Bound::Real(f64::NAN);
        for op in [Op::Equal, Op::Less, Op::LessOrEqual, Op::Greater, Op::GreaterOrEqual] {
            assert!(!excluded(op, &Bound::Real(1.0), Some(&nan), Some(&nan)));
        }
    }

    #[test]
    fn a_null_constant_has_no_bound() {
        assert!(Bound::of_value(&Value::Null).is_none());
        assert_eq!(Bound::of_value(&Value::Integer(7)), Some(Bound::Int(7)));
    }

    /// The gap the module doc documents, pinned so that closing it is a test that changes rather
    /// than a behaviour that quietly appears. A timestamp constant is microseconds and a file's
    /// statistics are at whatever unit the file chose, so no test is made from one at all.
    #[test]
    fn a_timestamp_constant_makes_no_bound() {
        assert_eq!(Bound::of_value(&Value::Timestamp(1)), None);
        assert_eq!(Bound::of_value(&Value::Time(1)), None);
        assert_eq!(Bound::of_value(&Value::Date(1)), Some(Bound::Int(1)));
    }

    #[test]
    fn a_flipped_op_is_the_one_with_its_operands_the_other_way_round() {
        assert_eq!(Op::Less.flipped(), Op::Greater);
        assert_eq!(Op::GreaterOrEqual.flipped(), Op::LessOrEqual);
        assert_eq!(Op::Equal.flipped(), Op::Equal);
    }

    #[test]
    fn a_minimum_and_a_maximum_accumulate() {
        let lower = Bound::Int(4).smaller(Bound::Int(9));
        let upper = Bound::Int(4).larger(Bound::Int(9));
        assert_eq!(lower, Bound::Int(4));
        assert_eq!(upper, Bound::Int(9));
        // Across domains neither moves, because there is no order to move along.
        assert_eq!(Bound::Int(4).smaller(Bound::Bytes(Vec::new())), Bound::Int(4));
    }
}
