//! A row of values used as a hash table key.
//!
//! Grouping, `DISTINCT` and the set operations all ask the same question: have I seen this row
//! before. SQL's answer to that is not the same as its answer to `=`. Two nulls group together,
//! `SELECT DISTINCT` collapses two null rows into one, and `UNION` treats them as one row, while
//! `NULL = NULL` is null. That is why this type exists rather than the code using [`Value`]'s own
//! equality directly: the rule here is `IS NOT DISTINCT FROM` applied column by column, and writing
//! it once is what stops grouping and `DISTINCT` from disagreeing about the same row.
//!
//! Floats get the same treatment they get in the comparison kernel. Two NaNs are one value and
//! negative zero is zero, because a group nobody can find again is worse than a group that follows
//! IEEE, and because a `DISTINCT` that emits NaN twice is a result that changes with the order the
//! rows arrived in.

use std::hash::{Hash, Hasher};

use rudb_common::Value;

/// A row of values compared and hashed the way SQL groups rows.
#[derive(Debug, Clone)]
pub(crate) struct Key(pub(crate) Vec<Value>);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(&other.0).all(|(left, right)| same(left, right))
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for value in &self.0 {
            hash_value(value, state);
        }
    }
}

/// Whether two values group together.
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(a), Value::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
        (Value::Double(a), Value::Double(b)) => a == b || (a.is_nan() && b.is_nan()),
        _ => left == right,
    }
}

/// Hashes a value so that two values [`same`] considers equal hash the same.
///
/// The general case goes through `Display`, which is slow and is the honest tier 0 answer: a group
/// key is a `Value` today because rows are `Value`s today, and the row layout that section 7.4
/// describes, a fixed width prefix with the payload beside it, is what replaces both this and the
/// `Vec<Value>` it hashes. Every case that shows up in a ClickBench group key is written out above
/// that fallback, so the slow path is the nested types and the intervals.
fn hash_value<H: Hasher>(value: &Value, state: &mut H) {
    std::mem::discriminant(value).hash(state);
    match value {
        Value::Null => {}
        Value::Boolean(x) => x.hash(state),
        Value::TinyInt(x) => x.hash(state),
        Value::SmallInt(x) => x.hash(state),
        Value::Integer(x) | Value::Date(x) => x.hash(state),
        Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => x.hash(state),
        Value::HugeInt(x) => x.hash(state),
        Value::UTinyInt(x) => x.hash(state),
        Value::USmallInt(x) => x.hash(state),
        Value::UInteger(x) => x.hash(state),
        Value::UBigInt(x) => x.hash(state),
        Value::UHugeInt(x) => x.hash(state),
        Value::Float(x) => canonical(f64::from(*x)).hash(state),
        Value::Double(x) => canonical(*x).hash(state),
        Value::Varchar(x) => x.hash(state),
        Value::Blob(x) => x.hash(state),
        Value::Decimal { unscaled, width, scale } => {
            unscaled.hash(state);
            width.hash(state);
            scale.hash(state);
        }
        other => other.to_string().hash(state),
    }
}

/// The bit pattern a float hashes as, collapsing the two zeros and every NaN.
fn canonical(number: f64) -> u64 {
    if number.is_nan() {
        return f64::NAN.to_bits();
    }
    if number == 0.0 {
        return 0.0f64.to_bits();
    }
    number.to_bits()
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;

    use super::*;

    /// One key's hash, which is what the map buckets on and therefore what has to agree with `eq`.
    fn digest(key: &Key) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn key(values: &[Value]) -> Key {
        Key(values.to_vec())
    }

    /// The rule that separates grouping from `=`. If this were `Value`'s own equality then
    /// `GROUP BY x` over a column of nulls would produce one group per null row.
    #[test]
    fn two_nulls_are_one_group() {
        assert_eq!(key(&[Value::Null]), key(&[Value::Null]));
        assert_eq!(digest(&key(&[Value::Null])), digest(&key(&[Value::Null])));
    }

    #[test]
    fn a_null_and_a_value_are_not_one_group() {
        assert_ne!(key(&[Value::Null]), key(&[Value::Integer(0)]));
    }

    #[test]
    fn two_nans_are_one_group_and_the_two_zeros_are_one_group() {
        let nan = key(&[Value::Double(f64::NAN)]);
        assert_eq!(nan, key(&[Value::Double(f64::NAN)]));
        assert_eq!(digest(&nan), digest(&key(&[Value::Double(f64::NAN)])));
        let zero = key(&[Value::Double(0.0)]);
        let negative = key(&[Value::Double(-0.0)]);
        assert_eq!(zero, negative);
        assert_eq!(digest(&zero), digest(&negative));
    }

    #[test]
    fn a_wider_key_is_never_a_narrower_one() {
        assert_ne!(key(&[Value::Integer(1)]), key(&[Value::Integer(1), Value::Integer(1)]));
    }

    /// Equal keys have to hash equally or the map holds two entries for one group and the second
    /// one is never found again. This is the invariant, asserted over the shapes a group key takes.
    #[test]
    fn every_pair_that_groups_together_hashes_together() {
        let pairs = [
            (Value::Integer(7), Value::Integer(7)),
            (Value::Varchar("ada".to_string()), Value::Varchar("ada".to_string())),
            (Value::Double(1.5), Value::Double(1.5)),
            (Value::Float(f32::NAN), Value::Float(f32::NAN)),
            (Value::Boolean(true), Value::Boolean(true)),
            (
                Value::Decimal { unscaled: 125, width: 10, scale: 2 },
                Value::Decimal { unscaled: 125, width: 10, scale: 2 },
            ),
        ];
        for (left, right) in pairs {
            let left = key(&[left]);
            let right = key(&[right]);
            assert_eq!(left, right, "{left:?} and {right:?} should group together");
            assert_eq!(digest(&left), digest(&right), "{left:?} hashes apart from {right:?}");
        }
    }

    /// Two decimals that print the same and are stored differently are two values, and the general
    /// hash path goes through `Display`, so this is the case that would break it if the fallback
    /// covered decimals.
    #[test]
    fn a_decimal_is_keyed_on_what_it_stores_rather_than_on_how_it_prints() {
        let tenths = key(&[Value::Decimal { unscaled: 10, width: 10, scale: 1 }]);
        let hundredths = key(&[Value::Decimal { unscaled: 100, width: 10, scale: 2 }]);
        assert_ne!(tenths, hundredths);
    }
}
