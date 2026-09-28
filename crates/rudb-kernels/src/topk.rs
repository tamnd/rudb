//! `approx_top_k(x, k)`, the pin's Filtered Space-Saving, step for step.
//!
//! A group watches up to three times `k` values with a count each, kept in order of count with the
//! most counted first. A value already watched has its count raised and moves up past every value
//! with a strictly lower count. A new value takes a free place while there is one, and once there
//! is none it may take the place of the least counted value, which is the last one: its hash picks
//! a counter in a filter, and only when that counter plus one reaches the count of the last value
//! does the new value replace it, starting from that count, while the value it pushed out leaves
//! its count behind in its own filter counter. Otherwise the filter counter goes up and the value
//! is not watched yet.
//!
//! The pin tells values apart by their ascending sort key and hashes those bytes for the filter,
//! so this builds the same bytes and hashes them with the pin's hash, which is what makes a large
//! input with more distinct values than places come out the same as the pin's. The one change is
//! that the filter is only made when the places run out, where the pin makes and zeroes it up
//! front, which for a large `k` is most of the memory it takes and never changes an answer.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};

use rudb_common::{Error, Result, Value};

use crate::hash::bytes;
use crate::quantile::{Column, Whole};

/// How many values are watched for each one the call asks for.
const WATCHED: usize = 3;

/// How many filter counters there are for each watched value, before rounding up to a power of 2.
const FILTER_RATIO: usize = 8;

/// The largest `k` the pin takes is one below this.
const MOST_K: i64 = 1_000_000;

/// The byte in front of a value that is not null, in an ascending sort key with nulls last.
const VALID: u8 = 1;

/// The byte a null is in the same key.
const NULL: u8 = 2;

/// A group of `approx_top_k`.
#[derive(Debug, Clone, Default)]
pub(crate) struct TopK {
    /// How many values the answer holds at most, or 0 before the first value.
    k: usize,
    /// The watched values at fixed places, which `order` and `lookup` point into.
    stored: Vec<Entry>,
    /// Places in `stored`, most counted first.
    order: Vec<u32>,
    /// The place of each watched value, by the key [`watch_key`] builds.
    lookup: HashMap<Box<[u8]>, u32, PinHash>,
    /// The filter counters, empty until every place is taken.
    filter: Vec<u64>,
    /// The sort key of the value being counted, kept to save allocating one for each row.
    key: Vec<u8>,
}

/// A watched value.
#[derive(Debug, Clone)]
struct Entry {
    value: Value,
    hash: u64,
    count: u64,
    /// Where this entry is in `order`.
    index: u32,
}

impl TopK {
    /// Counts a value that is not null, with the `k` of its row for a group that has none yet.
    pub(crate) fn push(&mut self, value: &Value, k: Option<&Value>) -> Result<()> {
        self.start(k)?;
        let mut key = std::mem::take(&mut self.key);
        key.clear();
        watch_key(value, &mut key);
        self.count(&key, || canonical(value), 1);
        self.key = key;
        Ok(())
    }

    /// Counts a `VARCHAR` by its bytes, making the value only if it comes to be watched.
    pub(crate) fn push_text(&mut self, text: &[u8], k: Option<&Value>) -> Result<()> {
        self.start(k)?;
        self.count(text, || Value::Varchar(String::from_utf8_lossy(text).into_owned()), 1);
        Ok(())
    }

    /// Counts the row of a column a [`Column`] reads, building its key without making a value
    /// unless it comes to be watched.
    pub(crate) fn push_column(
        &mut self,
        column: Column<'_>,
        row: usize,
        k: Option<&Value>,
    ) -> Result<()> {
        self.start(k)?;
        let mut key = std::mem::take(&mut self.key);
        key.clear();
        key.push(VALID);
        match column {
            Column::Wholes(whole, numbers) => whole_key(whole, numbers.at(row), &mut key),
            Column::Reals(reals) => key.extend_from_slice(&double_bits(reals[row]).to_be_bytes()),
            Column::Flags(flags) => key.push(u8::from(flags[row])),
        }
        self.count(&key, || canonical(&column.value(row)), 1);
        self.key = key;
        Ok(())
    }

    /// Whether a value has come, so that `k` is known.
    pub(crate) const fn started(&self) -> bool {
        self.k > 0
    }

    /// Takes `k` from the first row that has a value, with the pin's checks.
    fn start(&mut self, k: Option<&Value>) -> Result<()> {
        if self.k > 0 {
            return Ok(());
        }
        let k = match k {
            None | Some(Value::Null) => {
                return Err(invalid("Invalid input for approx_top_k: k value cannot be NULL"));
            }
            Some(Value::BigInt(k)) => *k,
            Some(other) => {
                return Err(Error::internal(format!("approx_top_k with a k of {other:?}")));
            }
        };
        if k <= 0 {
            return Err(invalid("Invalid input for approx_top_k: k value must be > 0"));
        }
        if k >= MOST_K {
            return Err(invalid(&format!(
                "Invalid input for approx_top_k: k value must be < {MOST_K}"
            )));
        }
        #[expect(clippy::cast_possible_truncation, reason = "k is below a million")]
        #[expect(clippy::cast_sign_loss, reason = "k is above 0")]
        {
            self.k = k as usize;
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.k * WATCHED
    }

    /// Adds `increment` to the value whose sort key is `key`, watching it if it is not watched.
    fn count(&mut self, key: &[u8], value: impl FnOnce() -> Value, increment: u64) {
        if let Some(&place) = self.lookup.get(key) {
            self.raise(place, increment);
        } else {
            self.insert(key, bytes(key), value, increment);
        }
    }

    /// The pin's `InsertOrReplaceEntry`.
    fn insert(&mut self, key: &[u8], hash: u64, value: impl FnOnce() -> Value, increment: u64) {
        if self.order.len() < self.capacity() {
            let place = place(self.stored.len());
            self.stored.push(Entry { value: Value::Null, hash: 0, count: 0, index: place });
            self.order.push(place);
        }
        let Some(&place) = self.order.last() else { return };
        let last = &self.stored[place as usize];
        if last.count > 0 {
            if self.filter.is_empty() {
                self.filter = vec![0; (self.capacity() * FILTER_RATIO).next_power_of_two()];
            }
            let mask = self.filter.len() as u64 - 1;
            #[expect(clippy::cast_possible_truncation, reason = "the mask keeps it in the filter")]
            let slot = (hash & mask) as usize;
            if self.filter[slot] + increment < last.count {
                self.filter[slot] += increment;
                return;
            }
            #[expect(clippy::cast_possible_truncation, reason = "the mask keeps it in the filter")]
            let own = (last.hash & mask) as usize;
            self.filter[own] = last.count;
            let mut old = Vec::new();
            watch_key(&last.value, &mut old);
            self.lookup.remove(old.as_slice());
        }
        let entry = &mut self.stored[place as usize];
        entry.value = value();
        entry.hash = hash;
        self.lookup.insert(key.into(), place);
        self.raise(place, increment);
    }

    /// The pin's `IncrementCount`, which moves the value up past every one with a lower count.
    fn raise(&mut self, place: u32, increment: u64) {
        let entry = &mut self.stored[place as usize];
        entry.count += increment;
        let count = entry.count;
        let mut index = entry.index as usize;
        while index > 0 {
            let above = self.order[index - 1];
            if count <= self.stored[above as usize].count {
                break;
            }
            self.order.swap(index, index - 1);
            self.stored[above as usize].index = place_of(index);
            index -= 1;
        }
        self.stored[place as usize].index = place_of(index);
    }

    /// Takes in another group of the same call, the way the pin combines two.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        let Some(&last) = other.order.last() else { return Ok(()) };
        let least_theirs = other.stored[last as usize].count;
        let least_mine = if self.order.is_empty() {
            self.k = other.k;
            0
        } else {
            if self.k != other.k {
                return Err(Error::not_implemented(
                    "Approx Top K - cannot combine approx_top_K with different k values. K values \
                     must be the same for all entries within the same group"
                        .to_string(),
                ));
            }
            self.stored[self.order[self.order.len() - 1] as usize].count
        };
        let mut key = Vec::new();
        for index in 0..self.order.len() {
            let place = self.order[index];
            key.clear();
            watch_key(&self.stored[place as usize].value, &mut key);
            let increment = other
                .lookup
                .get(key.as_slice())
                .map_or(least_theirs, |&theirs| other.stored[theirs as usize].count);
            if increment > 0 {
                self.raise(place, increment);
            }
        }
        for &theirs in &other.order {
            let entry = &other.stored[theirs as usize];
            key.clear();
            watch_key(&entry.value, &mut key);
            if self.lookup.contains_key(key.as_slice()) {
                continue;
            }
            let count = entry.count + least_mine;
            let increment = if self.order.len() >= self.capacity() {
                let least = self.order.last().map_or(0, |&at| self.stored[at as usize].count);
                if count <= least {
                    continue;
                }
                count - least
            } else {
                count
            };
            self.insert(&key, entry.hash, || entry.value.clone(), increment);
        }
        if !other.filter.is_empty() {
            if self.filter.is_empty() {
                self.filter = vec![0; other.filter.len()];
            }
            for (mine, theirs) in self.filter.iter_mut().zip(&other.filter) {
                *mine += theirs;
            }
        }
        Ok(())
    }

    /// The most counted values, up to `k` of them, or null for a group that saw none.
    pub(crate) fn finish(&self, element: &rudb_common::LogicalType) -> Value {
        if self.order.is_empty() {
            return Value::Null;
        }
        let values = self
            .order
            .iter()
            .take(self.k)
            .map(|&place| self.stored[place as usize].value.clone())
            .collect();
        Value::List { element: element.clone(), values }
    }
}

fn invalid(message: &str) -> Error {
    Error::invalid_input(message.to_string())
}

#[expect(clippy::cast_possible_truncation, reason = "there are at most three million places")]
const fn place(at: usize) -> u32 {
    at as u32
}

const fn place_of(index: usize) -> u32 {
    place(index)
}

/// Hashes a sort key the way the pin hashes a string, so the map needs no hash of its own.
#[derive(Debug, Clone, Copy, Default)]
struct PinHash;

impl BuildHasher for PinHash {
    type Hasher = PinHasher;

    fn build_hasher(&self) -> PinHasher {
        PinHasher(0)
    }
}

/// The hasher [`PinHash`] builds. A slice writes its length and then its bytes, and only the
/// bytes count.
#[derive(Debug)]
struct PinHasher(u64);

impl Hasher for PinHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, data: &[u8]) {
        self.0 = bytes(data);
    }

    fn write_usize(&mut self, _: usize) {}
}

/// The value as the pin gives it back, which is read off its sort key, so every zero is `0.0` and
/// every NaN the one NaN.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Double(v) if *v == 0.0 => Value::Double(0.0),
        Value::Double(v) if v.is_nan() => Value::Double(f64::NAN),
        Value::Float(v) if *v == 0.0 => Value::Float(0.0),
        Value::Float(v) if v.is_nan() => Value::Float(f32::NAN),
        Value::List { element, values } => {
            Value::List { element: element.clone(), values: values.iter().map(canonical).collect() }
        }
        Value::Struct(fields) => Value::Struct(
            fields.iter().map(|(name, value)| (name.clone(), canonical(value))).collect(),
        ),
        other => other.clone(),
    }
}

/// The key a value is watched by, which is its own bytes for a `VARCHAR`, since the pin counts
/// text as it is, and its sort key for everything else.
fn watch_key(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Varchar(text) => out.extend_from_slice(text.as_bytes()),
        _ => sort_key(value, out),
    }
}

/// The pin's ascending sort key of a value with nulls last, as `create_sort_key` builds it.
pub(crate) fn sort_key(value: &Value, out: &mut Vec<u8>) {
    if value.is_null() {
        out.push(NULL);
        return;
    }
    out.push(VALID);
    match value {
        Value::Boolean(v) => out.push(u8::from(*v)),
        Value::TinyInt(v) => signed(i64::from(*v), 1, out),
        Value::SmallInt(v) => signed(i64::from(*v), 2, out),
        Value::Integer(v) | Value::Date(v) => signed(i64::from(*v), 4, out),
        Value::BigInt(v)
        | Value::Time(v)
        | Value::Timestamp(v)
        | Value::TimestampTz(v)
        | Value::TimestampS(v)
        | Value::TimestampMs(v)
        | Value::TimestampNs(v) => signed(*v, 8, out),
        #[expect(clippy::cast_sign_loss, reason = "the pin keeps a zoned time in 64 bits")]
        Value::TimeTz(v) => out.extend_from_slice(&(*v as u64).to_be_bytes()),
        Value::UTinyInt(v) => out.push(*v),
        Value::USmallInt(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::UInteger(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::UBigInt(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::HugeInt(v) => huge(*v, out),
        Value::UHugeInt(v) => out.extend_from_slice(&v.to_be_bytes()),
        Value::Float(v) => out.extend_from_slice(&float_bits(*v).to_be_bytes()),
        Value::Double(v) => out.extend_from_slice(&double_bits(*v).to_be_bytes()),
        #[expect(clippy::cast_possible_truncation, reason = "the width says it fits")]
        Value::Decimal { unscaled, width, .. } => match width {
            0..=4 => signed(*unscaled as i64, 2, out),
            5..=9 => signed(*unscaled as i64, 4, out),
            10..=18 => signed(*unscaled as i64, 8, out),
            _ => huge(*unscaled, out),
        },
        Value::Interval { months, days, micros } => {
            signed(i64::from(*months), 4, out);
            signed(i64::from(*days), 4, out);
            signed(*micros, 8, out);
        }
        Value::Varchar(text) => {
            out.extend(text.bytes().map(|byte| byte.wrapping_add(1)));
            out.push(0);
        }
        Value::Blob(data) | Value::Bit(data) => {
            for &byte in data {
                if byte <= 1 {
                    out.push(1);
                }
                out.push(byte);
            }
            out.push(0);
        }
        Value::List { values, .. } => {
            for value in values {
                sort_key(value, out);
            }
            out.push(0);
        }
        Value::Map { entries, .. } => {
            for (key, value) in entries {
                out.push(VALID);
                sort_key(key, out);
                sort_key(value, out);
            }
            out.push(0);
        }
        Value::Struct(fields) => {
            for (_, value) in fields {
                sort_key(value, out);
            }
        }
        other => {
            out.extend_from_slice(other.to_string().as_bytes());
            out.push(0);
        }
    }
}

/// A whole number of a column, in the width its type is stored in.
fn whole_key(whole: Whole, n: i64, out: &mut Vec<u8>) {
    #[expect(clippy::cast_possible_truncation, reason = "the type says it fits")]
    #[expect(clippy::cast_sign_loss, reason = "the type says it is not negative")]
    match whole {
        Whole::TinyInt => signed(n, 1, out),
        Whole::SmallInt => signed(n, 2, out),
        Whole::Integer | Whole::Date => signed(n, 4, out),
        Whole::BigInt | Whole::Time | Whole::Timestamp | Whole::TimestampTz => signed(n, 8, out),
        Whole::UTinyInt => out.push(n as u8),
        Whole::USmallInt => out.extend_from_slice(&(n as u16).to_be_bytes()),
        Whole::UInteger => out.extend_from_slice(&(n as u32).to_be_bytes()),
        Whole::Decimal { width, .. } => match width {
            0..=4 => signed(n, 2, out),
            5..=9 => signed(n, 4, out),
            _ => signed(n, 8, out),
        },
    }
}

/// The low `width` bytes of a signed number, big end first, with the sign bit flipped.
#[expect(clippy::cast_sign_loss, reason = "only the bits are written")]
fn signed(n: i64, width: usize, out: &mut Vec<u8>) {
    let bits = (n as u64).to_be_bytes();
    let bytes = &bits[8 - width..];
    out.push(bytes[0] ^ 0x80);
    out.extend_from_slice(&bytes[1..]);
}

#[expect(clippy::cast_possible_truncation, reason = "the two halves are taken apart on purpose")]
fn huge(n: i128, out: &mut Vec<u8>) {
    signed((n >> 64) as i64, 8, out);
    out.extend_from_slice(&(n as u64).to_be_bytes());
}

/// The pin's `Radix::EncodeDouble`.
fn double_bits(x: f64) -> u64 {
    if x == 0.0 {
        return 1 << 63;
    }
    if x.is_nan() {
        return u64::MAX;
    }
    if x > f64::MAX {
        return u64::MAX - 1;
    }
    if x < -f64::MAX {
        return 0;
    }
    let bits = x.to_bits();
    if bits < 1 << 63 { bits + (1 << 63) } else { !bits }
}

/// The pin's `Radix::EncodeFloat`.
fn float_bits(x: f32) -> u32 {
    if x == 0.0 {
        return 1 << 31;
    }
    if x.is_nan() {
        return u32::MAX;
    }
    if x > f32::MAX {
        return u32::MAX - 1;
    }
    if x < -f32::MAX {
        return 0;
    }
    let bits = x.to_bits();
    if bits & (1 << 31) == 0 { bits | (1 << 31) } else { !bits }
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    fn top(values: impl IntoIterator<Item = i64>, k: i64) -> Vec<Value> {
        let mut state = TopK::default();
        for value in values {
            state.push(&Value::BigInt(value), Some(&Value::BigInt(k))).unwrap();
        }
        match state.finish(&LogicalType::BigInt) {
            Value::List { values, .. } => values,
            _ => Vec::new(),
        }
    }

    fn big(values: &[i64]) -> Vec<Value> {
        values.iter().copied().map(Value::BigInt).collect()
    }

    #[test]
    fn keys_are_the_bytes_the_pin_builds() {
        let key = |value: Value| {
            let mut out = Vec::new();
            sort_key(&value, &mut out);
            out
        };
        assert_eq!(key(Value::BigInt(1)), [1, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(key(Value::Double(1.5)), [1, 0xbf, 0xf8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(key(Value::Double(-0.0)), [1, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(key(Value::Varchar("ab".into())), [1, b'b', b'c', 0]);
        let list = Value::List {
            element: LogicalType::Integer,
            values: vec![Value::Integer(1), Value::Integer(2)],
        };
        assert_eq!(key(list), [1, 1, 0x80, 0, 0, 1, 1, 0x80, 0, 0, 2, 0]);
    }

    #[test]
    fn a_group_answers_what_the_pin_answers() {
        assert_eq!(top((0..100).map(|x| x % 5), 3), big(&[0, 1, 2]));
        assert_eq!(top([3, 1, 2, 1, 3], 5), big(&[1, 3, 2]));
        assert_eq!(top(1..10, 2), big(&[7, 8]));
        assert_eq!(top((0..100_000).map(|x| x % 7000), 2), big(&[1984, 1922]));
        assert_eq!(top((0..20_000).map(|x| (x * 7919) % 5000), 2), big(&[349, 4134]));
        let alternating = std::iter::repeat_n(0, 10_000)
            .chain(std::iter::repeat_n(1, 100_000))
            .chain(std::iter::repeat_n(2, 10));
        assert_eq!(top(alternating, 3), big(&[1, 0, 2]));
    }

    #[test]
    fn text_is_counted_by_its_own_bytes_as_the_pin_counts_it() {
        let top_text = |values: Vec<i64>, k: i64| {
            let mut state = TopK::default();
            for x in values {
                state.push(&Value::Varchar(x.to_string()), Some(&Value::BigInt(k))).unwrap();
            }
            state.finish(&LogicalType::Varchar)
        };
        let text = |values: &[&str]| Value::List {
            element: LogicalType::Varchar,
            values: values.iter().map(|&value| Value::Varchar(value.into())).collect(),
        };
        assert_eq!(top_text((0..200_000).map(|x| x % 100).collect(), 3), text(&["70", "77", "87"]));
        let spread = (0..100_000).map(|x| (x * 7919) % 5000).collect();
        assert_eq!(top_text(spread, 2), text(&["377", "810"]));
    }

    #[test]
    fn a_bad_k_is_refused_at_the_first_value() {
        let mut state = TopK::default();
        let refused = |k: Value| TopK::default().push(&Value::BigInt(1), Some(&k)).is_err();
        assert!(refused(Value::Null));
        assert!(refused(Value::BigInt(0)));
        assert!(refused(Value::BigInt(1_000_000)));
        state.push(&Value::BigInt(1), Some(&Value::BigInt(999_999))).unwrap();
        assert!(state.filter.is_empty(), "the filter waits until the places run out");
    }

    #[test]
    fn combined_groups_count_what_both_saw() {
        let mut left = TopK::default();
        let mut right = TopK::default();
        for x in [1, 1, 2] {
            left.push(&Value::BigInt(x), Some(&Value::BigInt(2))).unwrap();
        }
        for x in [3, 3, 3, 2] {
            right.push(&Value::BigInt(x), Some(&Value::BigInt(2))).unwrap();
        }
        left.combine(&right).unwrap();
        assert_eq!(
            left.finish(&LogicalType::BigInt),
            Value::List { element: LogicalType::BigInt, values: big(&[3, 1]) }
        );
    }
}
