//! Primary keys and unique constraints, and the set of keys each one keeps to check a write with.
//!
//! A key is checked the way the pin checks it, against the rows the table holds once the write is
//! done. A row whose key has a null in it takes part in no key, so any number of them can sit in a
//! unique column, and a primary key cannot hold a null in the first place because its columns are
//! `NOT NULL`. What the table keeps per key is the encoded key of every row it holds, built the
//! first time a write needs it and kept up to date by appends, so a write of a few rows into a big
//! table is checked against a hash set rather than by reading the table again.

use std::collections::HashSet;
use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_vector::Chunk;

use crate::QualifiedName;

/// A `PRIMARY KEY` or a `UNIQUE` constraint over one or more columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The columns, by place in the table, in the order the constraint named them.
    pub columns: Vec<usize>,
    /// Whether this is the table's primary key rather than a unique constraint.
    pub primary: bool,
}

/// A `FOREIGN KEY`: columns of this table whose values, when none is null, have to be a key the
/// referenced table holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// The columns of this table, by place, in the order the constraint named them.
    pub columns: Vec<usize>,
    /// The table the key is held by, which can be this one.
    pub table: QualifiedName,
    /// The columns of that table, by place, paired with `columns` one for one. They are the
    /// columns of one of its keys, though not necessarily in that key's order.
    pub referenced: Vec<usize>,
}

/// One constraint of a table, by its place in the list of its kind, which is how a table keeps the
/// order its constraints were written in for `duckdb_constraints()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// One of the table's keys.
    Key(usize),
    /// One of the table's `CHECK` constraints.
    Check(usize),
    /// One of the table's foreign keys.
    Foreign(usize),
    /// The `NOT NULL` of the column at this place.
    NotNull(usize),
}

impl Key {
    /// The word the pin's message uses for the constraint.
    fn kind(&self) -> &'static str {
        if self.primary { "primary key" } else { "unique" }
    }
}

/// The encoded keys of every row a table holds, for one [`Key`].
///
/// Behind an [`Arc`] so that the copy of the catalog a transaction keeps to roll back to shares it,
/// and the first write after the copy is the one that pays for a set of its own.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen(Arc<Held>);

/// The keys themselves. A key of one integer column is kept as the integer, which is about a fifth
/// of the memory of its encoding in a box of its own. Built over the 36 million rows of the JOB
/// `cast_info`, the boxes alone were more than the 4 GB a load into it had.
#[derive(Debug, Clone, Default)]
struct Held {
    /// The keys of one `INTEGER` or `BIGINT` column.
    ints: Ints,
    /// Every other key, encoded.
    bytes: HashSet<Box<[u8]>>,
}

/// The keys of one integer column, mostly as a bitmap over the range they cover.
///
/// The keys of an `id` column are a run of integers with few gaps, and a bit a key holds them in a
/// sixteenth of what a hash set spends. Hashing them one at a time was also the one thread a load
/// into the JOB `person_info` ran on for fifteen seconds of a sixty five second `COPY`, and a bit
/// test is not. A key too far from the others for the bitmap to stretch to goes in the hash set,
/// so keys spread over the whole of `i64` cost what they always did.
#[derive(Debug, Clone, Default)]
struct Ints {
    /// The key the first bit stands for, a multiple of 64 so that every word starts on one.
    base: i64,
    bits: Vec<u64>,
    /// How many bits are set.
    set: usize,
    /// The keys outside the bitmap, which can be inside it later, once it has stretched.
    rest: HashSet<i64>,
}

/// How many bits the bitmap may spend on each key it holds, past [`SLACK`].
///
/// Sixteen bits is two bytes a key, an eighth of what the hash set spends. A column whose keys are
/// spread thinner than one in sixteen keeps the ones that do not fit in the hash set.
const SPREAD: usize = 16;

/// How many bits the bitmap may spend whatever it holds, so that the first few keys of a table
/// can start one.
const SLACK: usize = 1 << 16;

impl Ints {
    fn len(&self) -> usize {
        self.set + self.rest.len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The word and the bit of `key`, when the bitmap reaches it.
    fn slot(&self, key: i64) -> Option<(usize, u64)> {
        let offset = usize::try_from(i128::from(key) - i128::from(self.base)).ok()?;
        let word = offset / 64;
        (word < self.bits.len()).then(|| (word, 1u64 << (offset % 64)))
    }

    fn contains(&self, key: i64) -> bool {
        self.slot(key).is_some_and(|(word, bit)| self.bits[word] & bit != 0)
            || self.rest.contains(&key)
    }

    /// Adds a key and says whether it was new.
    fn insert(&mut self, key: i64) -> bool {
        if self.slot(key).is_none() {
            self.reach(key, key, 1);
        }
        let Some((word, bit)) = self.slot(key) else {
            return self.rest.insert(key);
        };
        if self.bits[word] & bit != 0 || self.rest.contains(&key) {
            return false;
        }
        self.bits[word] |= bit;
        self.set += 1;
        true
    }

    /// Adds the keys in order, stopping at the first one that was already held, and says where it
    /// was. The keys before it are added and the rest are not.
    fn insert_all(&mut self, keys: &[i64]) -> Option<usize> {
        let (Some(&low), Some(&high)) = (keys.iter().min(), keys.iter().max()) else {
            return None;
        };
        self.reach(low, high, keys.len());
        keys.iter().position(|&key| !self.insert(key))
    }

    /// Where the first of `keys` that is held is.
    fn first_held(&self, keys: &[i64]) -> Option<usize> {
        if self.is_empty() {
            return None;
        }
        keys.iter().position(|&key| self.contains(key))
    }

    /// Every key held, in no particular order.
    fn keys(&self) -> impl Iterator<Item = i64> + '_ {
        let base = i128::from(self.base);
        let set = self.bits.iter().enumerate().flat_map(move |(word, &bits)| {
            let first = base + 64 * word as i128;
            let next = |left: &u64| Some(left & left.wrapping_sub(1)).filter(|&left| left != 0);
            std::iter::successors(Some(bits).filter(|&bits| bits != 0), next)
                .map(move |left| (first + i128::from(left.trailing_zeros())) as i64)
        });
        set.chain(self.rest.iter().copied())
    }

    /// Stretches the bitmap over `low..=high`, about to take `adding` more keys, when that keeps it
    /// within [`SPREAD`] bits a key. Leaves it as it is when it does not.
    ///
    /// Stretched downwards by as much again as it already holds, when the budget has room for it,
    /// so that keys arriving in falling order a few at a time copy the bitmap a logarithmic number
    /// of times rather than once each. Upwards the vector's own doubling does the same.
    fn reach(&mut self, low: i64, high: i64, adding: usize) {
        let floor = |key: i128| key - key.rem_euclid(64);
        let (from, to) = if self.bits.is_empty() {
            (floor(i128::from(low)), i128::from(high))
        } else {
            let base = i128::from(self.base);
            let end = base + 64 * self.bits.len() as i128 - 1;
            (floor(i128::from(low)).min(base), i128::from(high).max(end))
        };
        let words = (to - from) / 64 + 1;
        let budget =
            ((self.len() + adding).saturating_mul(SPREAD).saturating_add(SLACK) / 64) as i128;
        if words > budget {
            return;
        }
        if self.bits.is_empty() {
            self.base = from as i64;
            self.bits = vec![0; words as usize];
            return;
        }
        let before = (i128::from(self.base) - from) / 64;
        if before > 0 {
            let lowest = (i128::from(i64::MIN) - from) / 64;
            let spare = (self.bits.len() as i128).min(budget - words).min(-lowest).max(0);
            let from = from - 64 * spare;
            let before = (before + spare) as usize;
            let mut bits = Vec::with_capacity(before + self.bits.len());
            bits.resize(before, 0);
            bits.extend_from_slice(&self.bits);
            self.bits = bits;
            self.base = from as i64;
        }
        let words = ((to - i128::from(self.base)) / 64 + 1) as usize;
        if words > self.bits.len() {
            self.bits.resize(words, 0);
        }
    }
}

/// What one row's key is: none for a key with a null in it, an integer, or the encoding written
/// into the caller's scratch buffer.
#[derive(Clone, Copy)]
pub(crate) enum Encoded {
    Null,
    Int(i64),
    Bytes,
}

/// The key of one row. The two integer types are the ones [`push`] writes as an `i64` behind the
/// same tag, so a key of one of them is the same key whichever set it lands in.
pub(crate) fn encode(chunk: &Chunk, key: &Key, row: usize, out: &mut Vec<u8>) -> Result<Encoded> {
    out.clear();
    if let &[column] = key.columns.as_slice() {
        let value = chunk.column(column)?.value_at(row);
        return Ok(match value {
            Value::Integer(v) => Encoded::Int(i64::from(v)),
            Value::BigInt(v) => Encoded::Int(v),
            value if push(&value, out) => Encoded::Bytes,
            _ => Encoded::Null,
        });
    }
    for &column in &key.columns {
        if !push(&chunk.column(column)?.value_at(row), out) {
            return Ok(Encoded::Null);
        }
    }
    Ok(Encoded::Bytes)
}

/// The keys of every row of the chunk as one block, for a key of one `INTEGER` or `BIGINT` column
/// with no null in it, which is what a primary key on an `id` is. False for every other key, which
/// is encoded a row at a time with [`encode`] instead. The block is the same keys `encode` gives,
/// in the same order, with the row a key is from its place in the block.
fn int_block(chunk: &Chunk, key: &Key, out: &mut Vec<i64>) -> Result<bool> {
    let &[column] = key.columns.as_slice() else {
        return Ok(false);
    };
    let vector = chunk.column(column)?;
    if !matches!(vector.logical_type(), LogicalType::Integer | LogicalType::BigInt)
        || !vector.never_null()
    {
        return Ok(false);
    }
    Ok(vector.signed_block(out) && out.len() == chunk.len())
}

impl Held {
    fn contains(&self, encoded: Encoded, scratch: &[u8]) -> bool {
        match encoded {
            Encoded::Null => false,
            Encoded::Int(v) => self.ints.contains(v),
            Encoded::Bytes => self.bytes.contains(scratch),
        }
    }

    /// Adds a key and says whether it was new. A null key always is.
    fn insert(&mut self, encoded: Encoded, scratch: &[u8]) -> bool {
        match encoded {
            Encoded::Null => true,
            Encoded::Int(v) => self.ints.insert(v),
            Encoded::Bytes => self.bytes.insert(scratch.into()),
        }
    }
}

/// One column of a key onto the end of its encoding, or false for a null.
pub(crate) fn push(value: &Value, out: &mut Vec<u8>) -> bool {
    match value {
        Value::Null => return false,
        Value::Varchar(text) => {
            out.push(b's');
            out.extend_from_slice(&(text.len() as u64).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        Value::Integer(v) => {
            out.push(b'i');
            out.extend_from_slice(&i64::from(*v).to_le_bytes());
        }
        Value::BigInt(v) => {
            out.push(b'i');
            out.extend_from_slice(&v.to_le_bytes());
        }
        // A float key is equal to itself whatever sign its zero has, which is what the pin's
        // comparison says too.
        Value::Double(0.0) | Value::Float(0.0) => out.extend_from_slice(b"d0"),
        other => {
            let text = format!("{other:?}");
            out.push(b'v');
            out.extend_from_slice(&(text.len() as u64).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
    }
    true
}

impl Key {
    /// The key of a row given as all of its values, or `None` when a column of it is null. Two rows
    /// have the same key exactly when this is the same for both.
    #[must_use]
    pub fn of_row(&self, row: &[Value]) -> Option<Box<[u8]>> {
        let mut out = Vec::new();
        for &column in &self.columns {
            if !push(&row[column], &mut out) {
                return None;
            }
        }
        Some(out.into_boxed_slice())
    }
}

/// `a: 1, b: x`, the way the pin names a key that is already there.
fn named(chunk: &Chunk, key: &Key, columns: &[Field], row: usize) -> Result<String> {
    let mut parts = Vec::with_capacity(key.columns.len());
    for &column in &key.columns {
        let value = chunk.column(column)?.value_at(row);
        parts.push(format!("{}: {value}", columns[column].name));
    }
    Ok(parts.join(", "))
}

/// `1, x`, the way the pin names a key written twice by one statement.
fn bare(chunk: &Chunk, key: &Key, row: usize) -> Result<String> {
    let mut parts = Vec::with_capacity(key.columns.len());
    for &column in &key.columns {
        parts.push(chunk.column(column)?.value_at(row).to_string());
    }
    Ok(parts.join(", "))
}

impl Seen {
    /// The keys of these rows, refused if one repeats. `fresh` says the rows are all of the table,
    /// which is how an `UPDATE` or a `DELETE` lands, and a repeat there is reported the way the pin
    /// reports a key that was already there.
    pub(crate) fn of(chunks: &[Chunk], key: &Key, columns: &[Field], fresh: bool) -> Result<Self> {
        let mut seen = Self::default();
        for chunk in chunks {
            seen.absorb(chunk, key, columns, fresh)?;
        }
        Ok(seen)
    }

    /// Adds the keys of one more chunk of the rows [`Self::of`] is given, so a caller reading a
    /// table a part at a time holds one part and the keys rather than the whole table.
    pub(crate) fn absorb(
        &mut self,
        chunk: &Chunk,
        key: &Key,
        columns: &[Field],
        fresh: bool,
    ) -> Result<()> {
        let held = Arc::make_mut(&mut self.0);
        let mut scratch = Vec::new();
        let mut block = Vec::new();
        let repeated = if int_block(chunk, key, &mut block)? {
            held.ints.insert_all(&block)
        } else {
            let mut repeated = None;
            for row in 0..chunk.len() {
                let encoded = encode(chunk, key, row, &mut scratch)?;
                if !held.insert(encoded, &scratch) {
                    repeated = Some(row);
                    break;
                }
            }
            repeated
        };
        if let Some(row) = repeated {
            return Err(if fresh {
                Error::constraint(format!(
                    "Duplicate key \"{}\" violates {} constraint.",
                    named(chunk, key, columns, row)?,
                    key.kind()
                ))
            } else {
                Error::constraint(format!(
                    "PRIMARY KEY or UNIQUE constraint violation: duplicate key \"{}\"",
                    bare(chunk, key, row)?
                ))
            });
        }
        Ok(())
    }

    /// Checks rows about to be appended against the ones held and against each other, and returns
    /// their keys for [`Self::extend`] to add once every key of the table has passed. Nothing is
    /// changed here, so a refusal leaves the set as it was.
    ///
    /// A key that is already held is found first, over all the new rows, and only then a key the
    /// new rows repeat among themselves, which is the order the pin finds them in.
    ///
    /// `committing` says the rows are a transaction's, going into the table it committed to, and a
    /// key already held is then named the way the pin names it when a commit fails.
    pub(crate) fn check(
        &self,
        chunks: &[Chunk],
        key: &Key,
        columns: &[Field],
        committing: bool,
    ) -> Result<Self> {
        let mut scratch = Vec::new();
        let mut block = Vec::new();
        for chunk in chunks {
            let held = if int_block(chunk, key, &mut block)? {
                self.0.ints.first_held(&block)
            } else {
                let mut held = None;
                for row in 0..chunk.len() {
                    let encoded = encode(chunk, key, row, &mut scratch)?;
                    if self.0.contains(encoded, &scratch) {
                        held = Some(row);
                        break;
                    }
                }
                held
            };
            if let Some(row) = held {
                return Err(Error::constraint(if committing {
                    format!(
                        "PRIMARY KEY or UNIQUE constraint violation: duplicate key \"{}\"",
                        bare(chunk, key, row)?
                    )
                } else {
                    format!(
                        "Duplicate key \"{}\" violates {} constraint.",
                        named(chunk, key, columns, row)?,
                        key.kind()
                    )
                }));
            }
        }
        Self::of(chunks, key, columns, false)
    }

    /// Refuses the first row of `chunks` whose key is held here and was not held in the set
    /// `before` gives, one this set grew from, in the pin's words for a key already there. That
    /// set is built only once a key is found here, which is the rare case.
    pub(crate) fn refuse_added(
        &self,
        before: impl FnOnce() -> Result<Self>,
        chunks: &[Chunk],
        key: &Key,
        columns: &[Field],
    ) -> Result<()> {
        let mut before = Some(before);
        let mut then: Option<Self> = None;
        let mut scratch = Vec::new();
        for chunk in chunks {
            for row in 0..chunk.len() {
                let encoded = encode(chunk, key, row, &mut scratch)?;
                if !self.0.contains(encoded, &scratch) {
                    continue;
                }
                if then.is_none() {
                    then = Some(before.take().expect("built once")()?);
                }
                if then.as_ref().is_some_and(|then| then.0.contains(encoded, &scratch)) {
                    continue;
                }
                return Err(Error::constraint(format!(
                    "Duplicate key \"{}\" violates {} constraint.",
                    named(chunk, key, columns, row)?,
                    key.kind()
                )));
            }
        }
        Ok(())
    }

    /// Adds the keys [`Self::check`] passed. The set is copied only when a transaction's copy of
    /// the catalog still shares it.
    pub(crate) fn extend(&mut self, added: Self) {
        // The first load into a table, where copying the keys into an empty set would hold them
        // twice at the peak.
        if self.0.ints.is_empty() && self.0.bytes.is_empty() {
            *self = added;
            return;
        }
        let held = Arc::make_mut(&mut self.0);
        let ints = &added.0.ints;
        let (low, high) = ints
            .keys()
            .fold((i64::MAX, i64::MIN), |(low, high), key| (low.min(key), high.max(key)));
        if !ints.is_empty() {
            held.ints.reach(low, high, ints.len());
        }
        for key in ints.keys() {
            held.ints.insert(key);
        }
        held.bytes.extend(added.0.bytes.iter().cloned());
    }
}

/// The keys of the rows a load streams into an empty table, kept as compactly as they can be so the
/// load is checked for a repeated key once, at the end, instead of holding its rows to do it.
///
/// A key of one integer column is eight bytes a row. Loading the JOB `cast_info`, 36 million rows
/// with an `id` primary key, through the table instead took more than the 4 GB of the machine it
/// ran on.
#[derive(Debug, Default)]
pub struct KeyLog {
    ints: Vec<i64>,
    bytes: Vec<Box<[u8]>>,
}

impl KeyLog {
    /// Notes the key of every row of the chunk. A key with a null in it is no key and is skipped.
    ///
    /// # Errors
    ///
    /// If the chunk has no column the key names.
    pub fn record(&mut self, chunk: &Chunk, key: &Key) -> Result<()> {
        let mut block = Vec::new();
        if int_block(chunk, key, &mut block)? {
            self.ints.extend_from_slice(&block);
            return Ok(());
        }
        let mut scratch = Vec::new();
        for row in 0..chunk.len() {
            match encode(chunk, key, row, &mut scratch)? {
                Encoded::Null => {}
                Encoded::Int(v) => self.ints.push(v),
                Encoded::Bytes => self.bytes.push(scratch.as_slice().into()),
            }
        }
        Ok(())
    }

    /// Takes in the keys another instance of the same load noted.
    pub fn merge(&mut self, other: Self) {
        self.ints.extend(other.ints);
        self.bytes.extend(other.bytes);
    }

    /// Whether any key was noted twice. Sorts what it holds to find out.
    pub fn repeats(&mut self) -> bool {
        self.ints.sort_unstable();
        self.bytes.sort_unstable();
        self.ints.windows(2).any(|pair| pair[0] == pair[1])
            || self.bytes.windows(2).any(|pair| pair[0] == pair[1])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::Ints;

    /// A small generator so the sequences below are the same on every run.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Feeds the keys to the set and to a hash set one at a time, and asks both the same questions
    /// after every key.
    fn agrees(keys: &[i64]) {
        let mut ints = Ints::default();
        let mut model = HashSet::new();
        for &key in keys {
            assert_eq!(ints.insert(key), model.insert(key), "inserting {key}");
            assert_eq!(ints.len(), model.len());
            assert!(ints.contains(key));
        }
        for &key in keys {
            for probe in [key.wrapping_sub(1), key, key.wrapping_add(1)] {
                assert_eq!(ints.contains(probe), model.contains(&probe), "probing {probe}");
            }
        }
        let mut held: Vec<i64> = ints.keys().collect();
        held.sort_unstable();
        let mut wanted: Vec<i64> = model.into_iter().collect();
        wanted.sort_unstable();
        assert_eq!(held, wanted);
    }

    #[test]
    fn a_set_of_integer_keys_holds_what_a_hash_set_holds() {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        agrees(&(1..5000).collect::<Vec<_>>());
        agrees(&(1..5000).rev().collect::<Vec<_>>());
        agrees(&(0..3000).map(|at| (at * 7919) % 2999 - 1500).collect::<Vec<_>>());
        agrees(&[i64::MIN, i64::MAX, 0, -1, 1, i64::MIN + 63, i64::MAX - 63, i64::MIN, i64::MAX]);
        agrees(&(0..2000).map(|_| next(&mut state) as i64).collect::<Vec<_>>());
        // Mostly a run, with keys far from it and repeats, which is a bitmap with a hash set beside
        // it that the bitmap later stretches over.
        let mixed = (0..20_000)
            .map(|at| match next(&mut state) % 10 {
                0 => (next(&mut state) % 4_000_000) as i64 - 2_000_000,
                1 => (next(&mut state) % 20_000) as i64,
                _ => at,
            })
            .collect::<Vec<_>>();
        agrees(&mixed);
    }

    #[test]
    fn a_block_of_keys_stops_at_the_first_one_already_there() {
        let mut ints = Ints::default();
        assert_eq!(ints.insert_all(&[5, 6, 7]), None);
        assert_eq!(ints.first_held(&[1, 2, 7, 5]), Some(2));
        assert_eq!(ints.insert_all(&[8, 9, 6, 10]), Some(2));
        // The keys before the repeat went in and the ones after it did not.
        assert!(ints.contains(8) && ints.contains(9) && !ints.contains(10));
        assert_eq!(ints.insert_all(&[11, 12, 11]), Some(2));
        assert_eq!(ints.first_held(&[100, -100]), None);
        assert_eq!(Ints::default().first_held(&[1]), None);
        assert_eq!(ints.insert_all(&[]), None);
    }

    #[test]
    fn keys_of_an_id_column_take_a_bit_each() {
        let mut ints = Ints::default();
        let keys = (1..=1_000_000).collect::<Vec<i64>>();
        for block in keys.chunks(2048) {
            assert_eq!(ints.insert_all(block), None);
        }
        assert!(ints.rest.is_empty());
        assert!(ints.bits.len() <= 1_000_000 / 64 + 2, "{} words", ints.bits.len());
        // A key a long way off does not stretch the bitmap to it.
        assert!(ints.insert(1 << 40));
        assert_eq!(ints.rest.len(), 1);
        assert!(ints.bits.len() <= 1_000_000 / 64 + 2);
        assert!(!ints.insert(1 << 40) && !ints.insert(500_000));
    }
}
