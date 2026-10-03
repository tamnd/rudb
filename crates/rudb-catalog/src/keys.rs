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

use rudb_common::{Error, Field, Result, Value};
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
    ints: HashSet<i64>,
    /// Every other key, encoded.
    bytes: HashSet<Box<[u8]>>,
}

/// What one row's key is: none for a key with a null in it, an integer, or the encoding written
/// into the caller's scratch buffer.
#[derive(Clone, Copy)]
enum Encoded {
    Null,
    Int(i64),
    Bytes,
}

/// The key of one row. The two integer types are the ones [`push`] writes as an `i64` behind the
/// same tag, so a key of one of them is the same key whichever set it lands in.
fn encode(chunk: &Chunk, key: &Key, row: usize, out: &mut Vec<u8>) -> Result<Encoded> {
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

impl Held {
    fn contains(&self, encoded: Encoded, scratch: &[u8]) -> bool {
        match encoded {
            Encoded::Null => false,
            Encoded::Int(v) => self.ints.contains(&v),
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
fn push(value: &Value, out: &mut Vec<u8>) -> bool {
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
        for row in 0..chunk.len() {
            let encoded = encode(chunk, key, row, &mut scratch)?;
            if held.insert(encoded, &scratch) {
                continue;
            }
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
        for chunk in chunks {
            for row in 0..chunk.len() {
                let encoded = encode(chunk, key, row, &mut scratch)?;
                if self.0.contains(encoded, &scratch) {
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
        held.ints.extend(added.0.ints.iter().copied());
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
