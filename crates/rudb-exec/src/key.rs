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

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hash, Hasher};

use rudb_common::Value;

/// A row of values compared and hashed the way SQL groups rows.
#[derive(Debug, Clone, Default)]
pub(crate) struct Key(pub(crate) Vec<Value>);

/// A map from a row to whatever is being counted about it.
pub(crate) type RowMap<V> = HashMap<Key, V, BuildHasherDefault<Digest>>;

/// A set of rows, which is what duplicate elimination and `DISTINCT` inside an aggregate both are.
pub(crate) type RowSet = HashSet<Key, BuildHasherDefault<Digest>>;

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

/// The hasher a table of rows is built on.
///
/// The standard library's default is SipHash, which is chosen to survive an attacker who gets to
/// pick the keys. Nobody picks the keys of a group by: they are the rows of a table that is already
/// on this machine, and the cost of the choice is paid on every one of them. A group by over a
/// hundred million rows hashes a hundred million times and SipHash is several times the price of
/// what is here, which is a multiply and a rotate per word.
///
/// The multiply leaves the entropy in the high bits and the standard table buckets on the low ones,
/// so [`Hasher::finish`] spreads them back before handing the value over. Without that last step a
/// key whose words differ only near the top lands in one bucket and the table degenerates into a
/// list.
///
/// This is not a defence against a chosen key. A query that groups on a column somebody else filled
/// can be made to collide, and the answer to that is a limit on what one query may take, which the
/// memory budget already is, rather than a hash nobody can predict.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Digest(u64);

/// An odd constant with a well spread bit pattern, which is all the multiply asks of it.
const ODD: u64 = 0x517c_c1b7_2722_0a95;

impl Digest {
    /// Folds one word into the running value.
    fn mix(&mut self, word: u64) {
        self.0 = mix(self.0, word);
    }
}

/// Folds one word into a running hash.
///
/// Free rather than a method because the grouping table in `table.rs` hashes a column at a time and
/// never builds a [`Digest`] at all, and two hash functions that are meant to be the same one are
/// two hash functions that will eventually differ. The rule lives here, where the equality it has to
/// agree with lives.
pub(crate) fn mix(state: u64, word: u64) -> u64 {
    (state.rotate_left(5) ^ word).wrapping_mul(ODD)
}

/// Moves the entropy a run of [`mix`] left in the high bits back down into the low ones.
///
/// Every table in here buckets on the low bits, and the multiply in `mix` pushes what it mixed the
/// other way, so a key whose words differ only near the top lands in one bucket without this.
pub(crate) fn spread(state: u64) -> u64 {
    let mut spread = state;
    spread ^= spread >> 32;
    spread = spread.wrapping_mul(ODD);
    spread ^= spread >> 29;
    spread
}

impl Hasher for Digest {
    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            self.mix(u64::from_le_bytes(word.try_into().unwrap_or([0; 8])));
        }
        let rest = words.remainder();
        if !rest.is_empty() {
            let mut last = [0; 8];
            last[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(last));
        }
        self.mix(bytes.len() as u64);
    }

    fn write_u8(&mut self, value: u8) {
        self.mix(u64::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.mix(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.mix(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.mix(value);
    }

    fn write_u128(&mut self, value: u128) {
        self.mix(value as u64);
        self.mix((value >> 64) as u64);
    }

    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }

    fn finish(&self) -> u64 {
        spread(self.0)
    }
}

/// Whether two values group together.
pub(crate) fn same(left: &Value, right: &Value) -> bool {
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
pub(crate) fn canonical(number: f64) -> u64 {
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
    use super::*;

    /// One key's hash, which is what the map buckets on and therefore what has to agree with `eq`.
    fn digest(key: &Key) -> u64 {
        let mut hasher = Digest::default();
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

    /// The standard table takes the bucket from the low bits of the hash, and a multiply pushes
    /// what it mixed towards the high ones. A key that only ever differs near the top is what a
    /// `BIGINT` column of identifiers looks like, and without the spreading step in `finish` every
    /// one of these lands in the same bucket and the table stops being a table.
    #[test]
    fn keys_that_differ_only_in_their_high_bits_land_in_different_buckets() {
        let buckets = 1024;
        let mut taken = HashSet::new();
        for step in 0..64i64 {
            let value = Value::BigInt((step + 1) << 40);
            taken.insert(digest(&key(&[value])) % buckets);
        }
        assert!(taken.len() > 55, "64 keys landed in {} of {buckets} buckets", taken.len());
    }

    /// The order of the columns is part of the key, or `GROUP BY a, b` would put `(1, 2)` and
    /// `(2, 1)` in one group whenever the two columns hold each other's values.
    #[test]
    fn the_same_values_in_a_different_order_are_a_different_key() {
        let forwards = key(&[Value::Integer(1), Value::Integer(2)]);
        let backwards = key(&[Value::Integer(2), Value::Integer(1)]);
        assert_ne!(forwards, backwards);
        assert_ne!(digest(&forwards), digest(&backwards));
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
