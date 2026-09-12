//! The hash table a group by probes, held column at a time.
//!
//! `key.rs` answers what one row is, and this answers how to find that row again quickly. The two
//! live next to each other rather than one replacing the other because they are asked different
//! questions. A `DISTINCT` inside an aggregate is handed one argument at a time and has nowhere
//! better to put it than a [`Value`]. A group by is handed a thousand rows that are already sitting
//! in vectors, and building a `Vec<Value>` per row in order to ask about it is where most of
//! ClickBench went: #237 measured `GROUP BY WatchID, ClientIP` at ten times the pinned binary, on a
//! key that never needed to leave the vectors it arrived in.
//!
//! So this table never sees a row. It is given the key vectors and a row number, it hashes a column
//! at a time into one word per row before the row loop starts, and it compares the key it stored
//! against the vector in place. A new group costs a push onto one flat vector per key column, which
//! is an allocation amortized over the groups rather than one `Vec<Value>` from the allocator for
//! every group in the answer.
//!
//! # What the hash has to agree with
//!
//! Two values that [`same`] calls one value have to hash to one word, or the table holds two entries
//! for one group and the second is never found again. That is the whole invariant, and the thing
//! that makes it delicate here is that the same column arrives in more than one form. A `VARCHAR`
//! read out of a parquet file is a dictionary in one chunk and flat in the next, and if the fast
//! path over a flat column and the general path over a dictionary disagreed by so much as a
//! discriminant, a group by would answer with the same string twice.
//!
//! [`fold`] is therefore written so that every form of a column produces the same word for the same
//! value. What that costs is the type tag: the general path does not mix the discriminant the way
//! [`Key`](crate::key::Key) does, because a typed arm over a run of `i32` has no discriminant to
//! mix. It is not needed. Every value in a column has that column's type, so within one column the
//! tag is a constant, and two key columns of different types are told apart by their position in the
//! fold and by the comparison that follows the probe.

use rudb_common::{Error, Result, Value};
use rudb_vector::{Data, Vector};

use crate::key::{canonical, mix, same, spread};
use crate::rows;

/// What a bucket holds when it holds nothing.
const EMPTY: u32 = u32::MAX;

/// The most groups one of these can hold.
///
/// A slot is a `u32` because the buckets are most of what a probe reads and half of them being
/// padding would halve the number that fit in a cache line. The bound that leaves is four billion
/// groups, which at the width of a key is a hundred gigabytes of them, so a query that reaches it
/// has run out of memory in every sense that matters and the only question is which error says so.
const LIMIT: usize = EMPTY as usize;

/// How many buckets a table starts with.
///
/// Small, because most group bys in a query have few groups, and the doubling gets to a large table
/// in the twenty steps that a table with many of them needs.
const FIRST: usize = 64;

/// The word a null contributes to the hash.
///
/// A constant rather than nothing at all, so that a null in a column of zeroes does not hash as a
/// zero. It can collide with a real value that happens to be this pattern, which costs one
/// comparison and no correctness, because the comparison is what decides.
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

/// A hash table from a row of key columns to the slot its group was given.
///
/// The slot is the number of groups seen before this one, so the answer comes out in the order the
/// groups were first seen, which is what the operator above this relies on and what makes a failing
/// test a diff rather than an investigation.
#[derive(Debug)]
pub(crate) struct Table {
    /// One slot per bucket, [`EMPTY`] where there is none. A power of two long, so the bucket a
    /// hash belongs to is a mask rather than a division.
    buckets: Vec<u32>,
    /// The stored keys, column at a time. `columns[column][slot]` is one group's value in one key
    /// column, which is the layout that lets a group be pushed without asking the allocator for a
    /// row to put it in.
    columns: Vec<Vec<Value>>,
    /// The hash of each group's key, so that a probe compares one word before it compares a key.
    /// Worth its eight bytes on a string key, where the comparison it avoids is a memcmp.
    hashes: Vec<u64>,
    /// What the stored keys own away from themselves, which is the strings and blobs among them.
    owned: u64,
}

/// What a probe found, which is either a group or the bucket a new one goes in.
///
/// The bucket is handed back rather than looked for again by [`Table::insert`] because the probe
/// has already walked to it, and walking a second time is the cost of the first one over again on
/// exactly the rows where the table is about to grow.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Probe {
    /// The row's key is the group in this slot.
    Found(usize),
    /// The row's key is not in the table, and this is the bucket it would go in.
    Vacant(usize),
}

impl Table {
    /// An empty table over a key of `columns` columns.
    pub(crate) fn new(columns: usize) -> Self {
        Self {
            buckets: vec![EMPTY; FIRST],
            columns: vec![Vec::new(); columns],
            hashes: Vec::new(),
            owned: 0,
        }
    }

    /// How many groups are in it.
    pub(crate) fn len(&self) -> usize {
        self.hashes.len()
    }

    /// What the stored keys own away from themselves.
    ///
    /// Charged separately from [`Self::footprint`] because it is charged for longer. The strings a
    /// key holds move out of this table and into the rows the answer is built from, and survive it,
    /// while everything `footprint` counts is given back when the table goes.
    pub(crate) fn owned(&self) -> u64 {
        self.owned
    }

    /// What this has taken from the allocator, capacity rather than length in all three parts.
    ///
    /// Capacity for the reason #227 gives. A `Vec` doubles and so sits between half empty and full,
    /// and charging the used half of a structure that paid for all of it is how a query passes a
    /// limit it was told it was inside of.
    pub(crate) fn footprint(&self) -> u64 {
        let buckets = self.buckets.capacity() * size_of::<u32>();
        let hashes = self.hashes.capacity() * size_of::<u64>();
        let keys: usize =
            self.columns.iter().map(|column| column.capacity() * size_of::<Value>()).sum();
        u64::try_from(buckets + hashes + keys).unwrap_or(u64::MAX)
    }

    /// Looks for the key that `keys` holds at `row`.
    ///
    /// `hash` is that row's entry in what [`hash`] built for the chunk. Passing it in rather than
    /// computing it here is the point of the whole arrangement: the hash of a column of a thousand
    /// rows is one pass over a run of `i32` with the type dispatch done once, and doing it per row
    /// inside the probe would put the dispatch back.
    pub(crate) fn probe(&self, hash: u64, keys: &[Vector], row: usize) -> Probe {
        let mask = self.buckets.len() - 1;
        let mut at = (hash as usize) & mask;
        loop {
            let slot = self.buckets[at];
            if slot == EMPTY {
                return Probe::Vacant(at);
            }
            let slot = slot as usize;
            if self.hashes[slot] == hash && self.holds(slot, keys, row) {
                return Probe::Found(slot);
            }
            at = (at + 1) & mask;
        }
    }

    /// Adds the key that `keys` holds at `row` in the bucket a probe of the same row left vacant.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] at [`LIMIT`] groups.
    pub(crate) fn insert(
        &mut self,
        bucket: usize,
        hash: u64,
        keys: &[Vector],
        row: usize,
    ) -> Result<usize> {
        let slot = self.hashes.len();
        if slot >= LIMIT {
            return Err(Error::out_of_memory(format!(
                "a single group by cannot hold more than {LIMIT} groups"
            )));
        }
        for (at, column) in keys.iter().enumerate() {
            let value = column.value_at(row);
            // What it owns and not what it is. The `Value` itself is in one of the column vectors
            // below, whose capacity `footprint` counts, and counting it here as well would charge
            // every group twice for the part of it that is not a string.
            self.owned += rows::owned(&value);
            self.columns[at].push(value);
        }
        self.hashes.push(hash);
        self.buckets[bucket] = slot as u32;
        // Half full rather than the seven eighths a `HashMap` allows, because this probes linearly
        // and a linear probe at seven eighths walks a run of about eight buckets to find a miss.
        // The buckets are four bytes each, so the room the other half costs is small next to the
        // keys beside it.
        if self.hashes.len() * 2 >= self.buckets.len() {
            self.regrow();
        }
        Ok(slot)
    }

    /// Whether the group in `slot` has the key that `keys` holds at `row`.
    ///
    /// The string case is the one worth writing out. [`Vector::value_at`] on a `VARCHAR` allocates a
    /// `String` per call, and this is called at least once per input row of a group by over one, so
    /// the comparison reads the bytes where they already are. Everything else owns nothing, so
    /// `value_at` on it is a copy of a few bytes and going through [`same`] keeps the one definition
    /// of what groups together.
    ///
    /// [`Vector::text_at`] hands back nothing for the forms that do not store their text per
    /// position, and a caller that reads that as a difference is a caller that never finds a group
    /// again. A nested loop join makes its left side constant vectors, so a group by on a string
    /// column from the left of a join is exactly that case, and it answered with one group per row.
    /// So nothing from `text_at` means fall through to `value_at`, which is right for every form.
    fn holds(&self, slot: usize, keys: &[Vector], row: usize) -> bool {
        for (at, column) in keys.iter().enumerate() {
            let stored = &self.columns[at][slot];
            if let Value::Varchar(text) = stored {
                if let Some(borrowed) = column.text_at(row) {
                    if borrowed != text.as_str() {
                        return false;
                    }
                    continue;
                }
            }
            if !same(stored, &column.value_at(row)) {
                return false;
            }
        }
        true
    }

    /// Doubles the buckets and puts every group back in one.
    ///
    /// The keys do not move and are not looked at. A rehash reads the hash of each group, which is
    /// stored, so growing a table of seventeen million string keys touches no strings.
    fn regrow(&mut self) {
        let mut buckets = vec![EMPTY; self.buckets.len() * 2];
        let mask = buckets.len() - 1;
        for (slot, &hash) in self.hashes.iter().enumerate() {
            let mut at = (hash as usize) & mask;
            while buckets[at] != EMPTY {
                at = (at + 1) & mask;
            }
            buckets[at] = slot as u32;
        }
        self.buckets = buckets;
    }

    /// One key column of every group, in slot order.
    ///
    /// This is the whole reason the keys are stored a column at a time rather than a row at a time.
    /// A chunk of the answer wants a column, so the operator above cuts a range out of this and
    /// hands it to a vector, and no group is ever a row of its own on the way out.
    ///
    /// # Panics
    ///
    /// If `at` is not a column of the key this table was built over, which is a bug in the caller.
    pub(crate) fn column(&self, at: usize) -> &[Value] {
        &self.columns[at]
    }
}

/// Hashes a chunk of key columns into one word per row.
///
/// This is the column at a time half of #237. The type of a column is matched on once per column
/// per chunk rather than once per value, so the hash of a thousand rows of `INTEGER` is a pass over
/// a run of `i32` with a multiply and a rotate in it.
///
/// `hashes` is the caller's buffer, kept between chunks so that this does not go to the allocator
/// once per chunk either.
pub(crate) fn hash(keys: &[Vector], rows: usize, hashes: &mut Vec<u64>) {
    hashes.clear();
    hashes.resize(rows, 0);
    for column in keys {
        fold(column, rows, hashes);
    }
    for state in hashes.iter_mut() {
        *state = spread(*state);
    }
}

/// Folds one key column's values into the running hash of every row.
///
/// Every arm here has to produce what the general path at the bottom produces for the same value,
/// because the same column arrives flat in one chunk and as a dictionary in the next. The module
/// comment has the argument. The arms that are missing, which are the intervals and the nested
/// types, are missing on purpose: they fall through to the general path in every form, so there is
/// nothing for them to disagree with.
fn fold(column: &Vector, rows: usize, hashes: &mut [u64]) {
    let validity = column.validity();
    /// One pass over a run of values, turning each into a word the same way the general path does.
    macro_rules! run {
        ($values:expr, $word:expr) => {{
            let values = $values.as_slice();
            let word = $word;
            for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                let one = match values.get(row) {
                    Some(value) if validity.is_valid(row) => word(*value),
                    _ => NOTHING,
                };
                *state = mix(*state, one);
            }
            return;
        }};
    }
    if let Some(data) = column.data() {
        match data {
            Data::Bool(values) => run!(values, |x: bool| u64::from(x)),
            Data::Int8(values) => run!(values, |x: i8| i64::from(x) as u64),
            Data::Int16(values) => run!(values, |x: i16| i64::from(x) as u64),
            Data::Int32(values) => run!(values, |x: i32| i64::from(x) as u64),
            Data::Int64(values) => run!(values, |x: i64| x as u64),
            Data::UInt8(values) => run!(values, |x: u8| u64::from(x)),
            Data::UInt16(values) => run!(values, |x: u16| u64::from(x)),
            Data::UInt32(values) => run!(values, |x: u32| u64::from(x)),
            Data::UInt64(values) => run!(values, |x: u64| x),
            Data::Float32(values) => run!(values, |x: f32| canonical(f64::from(x))),
            Data::Float64(values) => run!(values, canonical),
            // The bytes and not the string, so that a `BLOB` whose bytes are not text hashes as
            // what it is rather than as a null.
            Data::Varlen(strings) => {
                for (row, state) in hashes.iter_mut().enumerate().take(rows) {
                    let one = match strings.bytes(row) {
                        Some(bytes) if validity.is_valid(row) => bytes_word(bytes),
                        _ => NOTHING,
                    };
                    *state = mix(*state, one);
                }
                return;
            }
            _ => {}
        }
    }
    // row at a time: every other form and every type without an arm above. A dictionary is read
    // through `text_at` where it can be, because `value_at` on a dictionary of strings copies one
    // per row and the form exists so that it does not have to. What is left after that is the
    // nested types and the intervals, which have no run of fixed width words to walk at all.
    for (row, state) in hashes.iter_mut().enumerate().take(rows) {
        *state = match column.text_at(row) {
            Some(text) => mix(*state, bytes_word(text.as_bytes())),
            None => fold_value(*state, &column.value_at(row)),
        };
    }
}

/// Folds one value into a running hash, for the forms and types that have no run to walk.
///
/// The nested types and the intervals go through `Display`, which is slow and is the same honest
/// answer `key.rs` gives: a group key is a `Value` until section 7.4's row layout replaces it, and
/// every type that shows up in a ClickBench group key is written out above that fallback.
fn fold_value(state: u64, value: &Value) -> u64 {
    match value {
        Value::Null => mix(state, NOTHING),
        Value::Boolean(x) => mix(state, u64::from(*x)),
        Value::TinyInt(x) => mix(state, i64::from(*x) as u64),
        Value::SmallInt(x) => mix(state, i64::from(*x) as u64),
        Value::Integer(x) | Value::Date(x) => mix(state, i64::from(*x) as u64),
        Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) => mix(state, *x as u64),
        Value::UTinyInt(x) => mix(state, u64::from(*x)),
        Value::USmallInt(x) => mix(state, u64::from(*x)),
        Value::UInteger(x) => mix(state, u64::from(*x)),
        Value::UBigInt(x) => mix(state, *x),
        Value::Float(x) => mix(state, canonical(f64::from(*x))),
        Value::Double(x) => mix(state, canonical(*x)),
        Value::Varchar(x) => mix(state, bytes_word(x.as_bytes())),
        Value::Blob(x) => mix(state, bytes_word(x)),
        // Two words, low first, the way the 128 bit layouts are read. The width and the scale of a
        // decimal are not mixed, because they are the column's and not the value's, and a flat
        // decimal column is a run of integers with no room to keep them.
        Value::HugeInt(x) | Value::Decimal { unscaled: x, .. } => {
            mix(mix(state, *x as u64), (*x >> 64) as u64)
        }
        Value::UHugeInt(x) => mix(mix(state, *x as u64), (*x >> 64) as u64),
        other => mix(state, bytes_word(other.to_string().as_bytes())),
    }
}

/// A run of bytes as one word, the way [`Digest`](crate::key::Digest) reads one.
///
/// The length goes in as well, so that `ab` and `ab\0` are two values rather than one.
fn bytes_word(bytes: &[u8]) -> u64 {
    let mut state = 0u64;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        state = mix(state, u64::from_le_bytes(word.try_into().unwrap_or([0; 8])));
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        state = mix(state, u64::from_le_bytes(last));
    }
    mix(state, bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    /// The hash of one column of values, in whatever form the vector is in.
    fn hashed(column: &Vector) -> Vec<u64> {
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(column), column.len(), &mut hashes);
        hashes
    }

    fn flat(ty: LogicalType, values: &[Value]) -> Vector {
        Vector::from_values(ty, values).expect("a flat vector of these values")
    }

    /// The invariant the whole module rests on. A column read out of a parquet file is a dictionary
    /// in one chunk and flat in the next, and a group by that hashed the two differently would put
    /// the same string in two groups and return it twice.
    #[test]
    fn a_dictionary_hashes_the_same_as_the_flat_column_it_stands_for() {
        let long = "lovelace, and a string past the sixteen bytes a view holds inline";
        let values = [
            Value::Varchar("ada".into()),
            Value::Varchar(String::new()),
            Value::Null,
            Value::Varchar(long.into()),
        ];
        let plain = flat(LogicalType::Varchar, &values);
        let distinct = flat(LogicalType::Varchar, &values);
        let dictionary =
            Vector::dictionary(vec![0, 1, 2, 3], distinct).expect("a dictionary of those values");
        assert_eq!(hashed(&plain), hashed(&dictionary));
    }

    /// The same argument for the other two forms, which a literal and a `range` produce.
    #[test]
    fn a_constant_and_a_sequence_hash_the_same_as_the_values_they_stand_for() {
        let constant = Vector::constant(LogicalType::Integer, Value::Integer(7), 3);
        let plain = flat(LogicalType::Integer, &vec![Value::Integer(7); 3]);
        assert_eq!(hashed(&constant), hashed(&plain));

        let sequence = Vector::sequence(10, 2, 4);
        let counted = flat(
            LogicalType::BigInt,
            &[Value::BigInt(10), Value::BigInt(12), Value::BigInt(14), Value::BigInt(16)],
        );
        assert_eq!(hashed(&sequence), hashed(&counted));
    }

    /// Two NaNs are one group and the two zeros are one group, the same rule `key.rs` applies,
    /// because a group nobody can find again is worse than a group that follows IEEE.
    #[test]
    fn the_floats_that_group_together_hash_together() {
        let left = flat(LogicalType::Double, &[Value::Double(f64::NAN), Value::Double(0.0)]);
        let right = flat(LogicalType::Double, &[Value::Double(-f64::NAN), Value::Double(-0.0)]);
        assert_eq!(hashed(&left), hashed(&right));
    }

    /// A null is a word of its own rather than nothing at all, or a null would group with a zero.
    #[test]
    fn a_null_does_not_hash_as_a_zero() {
        let nulls = flat(LogicalType::BigInt, &[Value::Null]);
        let zeroes = flat(LogicalType::BigInt, &[Value::BigInt(0)]);
        assert_ne!(hashed(&nulls), hashed(&zeroes));
    }

    /// The order of the columns is part of the key, or `GROUP BY a, b` would put `(1, 2)` and
    /// `(2, 1)` in one group whenever the two columns held each other's values.
    #[test]
    fn the_same_values_in_a_different_order_hash_apart() {
        let ones = flat(LogicalType::Integer, &[Value::Integer(1)]);
        let twos = flat(LogicalType::Integer, &[Value::Integer(2)]);
        let mut forwards = Vec::new();
        let mut backwards = Vec::new();
        hash(&[ones.clone(), twos.clone()], 1, &mut forwards);
        hash(&[twos, ones], 1, &mut backwards);
        assert_ne!(forwards, backwards);
    }

    /// What the table is for, end to end: the same key finds the same slot and a different one does
    /// not, over a key of two columns of different types.
    #[test]
    fn a_key_that_has_been_seen_is_found_and_a_new_one_is_not() {
        let names = flat(
            LogicalType::Varchar,
            &[Value::Varchar("ada".into()), Value::Varchar("ada".into()), Value::Null],
        );
        let numbers =
            flat(LogicalType::Integer, &[Value::Integer(1), Value::Integer(1), Value::Integer(1)]);
        let keys = [names, numbers];
        let mut hashes = Vec::new();
        hash(&keys, 3, &mut hashes);

        let mut table = Table::new(2);
        let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        let slot = table.insert(bucket, hashes[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(hashes[1], &keys, 1), Probe::Found(found) if found == slot));
        assert!(matches!(table.probe(hashes[2], &keys, 2), Probe::Vacant(_)));
        assert_eq!(table.len(), 1);
    }

    /// A string column that is a constant vector is still a string column. The hash of it already
    /// agreed with the flat form, and the comparison after the probe did not, so every row of a
    /// group by on the left side of a join opened a group of its own and the answer had the same
    /// string in it once per row.
    #[test]
    fn a_group_is_found_again_when_its_string_key_arrives_as_a_constant() {
        let long = "lovelace, and a string past the sixteen bytes a view holds inline";
        for text in ["ada", "", long] {
            let names = Vector::constant(LogicalType::Varchar, Value::Varchar(text.into()), 2);
            let keys = [names];
            let mut hashes = Vec::new();
            hash(&keys, 2, &mut hashes);

            let mut table = Table::new(1);
            let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
                panic!("an empty table found a group");
            };
            let slot = table.insert(bucket, hashes[0], &keys, 0).expect("room for one group");
            assert!(
                matches!(table.probe(hashes[1], &keys, 1), Probe::Found(found) if found == slot),
                "{text:?} did not find itself"
            );
            assert_eq!(table.len(), 1);
        }
    }

    /// The other half of it, which is that falling through to `value_at` did not make everything
    /// one group. A constant of one string and a flat column of another are two groups.
    #[test]
    fn two_constants_of_different_strings_are_still_two_groups() {
        let ada = Vector::constant(LogicalType::Varchar, Value::Varchar("ada".into()), 1);
        let grace = Vector::constant(LogicalType::Varchar, Value::Varchar("grace".into()), 1);
        let mut first = Vec::new();
        let mut second = Vec::new();
        hash(std::slice::from_ref(&ada), 1, &mut first);
        hash(std::slice::from_ref(&grace), 1, &mut second);

        let mut table = Table::new(1);
        let keys = [ada];
        let Probe::Vacant(bucket) = table.probe(first[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        table.insert(bucket, first[0], &keys, 0).expect("room for one group");
        assert!(matches!(table.probe(second[0], &[grace], 0), Probe::Vacant(_)));
    }

    /// Growing is where a table stops working quietly. Every key put in before a rehash has to be
    /// found after it, so this puts in more than the sixty four it starts with.
    #[test]
    fn every_group_is_still_found_after_the_buckets_have_doubled() {
        let values: Vec<Value> = (0..1000).map(Value::BigInt).collect();
        let column = flat(LogicalType::BigInt, &values);
        let keys = [column];
        let mut hashes = Vec::new();
        hash(&keys, values.len(), &mut hashes);

        let mut table = Table::new(1);
        for (row, &one) in hashes.iter().enumerate() {
            let Probe::Vacant(bucket) = table.probe(one, &keys, row) else {
                panic!("row {row} was found before it was inserted");
            };
            let slot = table.insert(bucket, one, &keys, row).expect("room");
            assert_eq!(slot, row);
        }
        assert_eq!(table.len(), values.len());
        for (row, &one) in hashes.iter().enumerate() {
            assert!(
                matches!(table.probe(one, &keys, row), Probe::Found(slot) if slot == row),
                "row {row} was lost by a rehash"
            );
        }
        assert_eq!(table.column(0).len(), values.len());
        assert_eq!(table.column(0)[7], Value::BigInt(7));
    }

    /// The strings a key holds are charged, and they are charged once the group is in rather than
    /// per row, since a row that is not a new group copies nothing.
    #[test]
    fn what_the_keys_own_is_counted() {
        let long = "a string well past the sixteen bytes a view holds inline".to_string();
        let column = flat(LogicalType::Varchar, &[Value::Varchar(long.clone())]);
        let keys = [column];
        let mut hashes = Vec::new();
        hash(&keys, 1, &mut hashes);
        let mut table = Table::new(1);
        assert_eq!(table.owned(), 0);
        let Probe::Vacant(bucket) = table.probe(hashes[0], &keys, 0) else {
            panic!("an empty table found a group");
        };
        table.insert(bucket, hashes[0], &keys, 0).expect("room");
        assert!(table.owned() >= long.len() as u64, "{} is not the string", table.owned());
    }
}
