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

/// A `PRIMARY KEY` or a `UNIQUE` constraint over one or more columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The columns, by place in the table, in the order the constraint named them.
    pub columns: Vec<usize>,
    /// Whether this is the table's primary key rather than a unique constraint.
    pub primary: bool,
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
pub(crate) struct Seen(Arc<HashSet<Box<[u8]>>>);

/// The key of one row, or `None` when a column of it is null.
fn encode(chunk: &Chunk, key: &Key, row: usize, out: &mut Vec<u8>) -> Result<bool> {
    out.clear();
    for &column in &key.columns {
        let value = chunk.column(column)?.value_at(row);
        match value {
            Value::Null => return Ok(false),
            Value::Varchar(text) => {
                out.push(b's');
                out.extend_from_slice(&(text.len() as u64).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Value::Integer(v) => {
                out.push(b'i');
                out.extend_from_slice(&i64::from(v).to_le_bytes());
            }
            Value::BigInt(v) => {
                out.push(b'i');
                out.extend_from_slice(&v.to_le_bytes());
            }
            // A float key is equal to itself whatever sign its zero has, which is what the pin's
            // comparison says too.
            Value::Double(0.0) => out.extend_from_slice(b"d0"),
            Value::Float(0.0) => out.extend_from_slice(b"d0"),
            other => {
                let text = format!("{other:?}");
                out.push(b'v');
                out.extend_from_slice(&(text.len() as u64).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
    Ok(true)
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
        let mut seen = HashSet::new();
        let mut scratch = Vec::new();
        for chunk in chunks {
            for row in 0..chunk.len() {
                if !encode(chunk, key, row, &mut scratch)? {
                    continue;
                }
                if !seen.insert(scratch.clone().into_boxed_slice()) {
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
            }
        }
        Ok(Self(Arc::new(seen)))
    }

    /// Checks rows about to be appended against the ones held and against each other, and returns
    /// the set the table holds once they are in. Nothing is changed when a key repeats.
    ///
    /// A key that is already held is found first, over all the new rows, and only then a key the
    /// new rows repeat among themselves, which is the order the pin finds them in.
    pub(crate) fn with(&self, chunks: &[Chunk], key: &Key, columns: &[Field]) -> Result<Self> {
        let mut scratch = Vec::new();
        for chunk in chunks {
            for row in 0..chunk.len() {
                if encode(chunk, key, row, &mut scratch)? && self.0.contains(scratch.as_slice()) {
                    return Err(Error::constraint(format!(
                        "Duplicate key \"{}\" violates {} constraint.",
                        named(chunk, key, columns, row)?,
                        key.kind()
                    )));
                }
            }
        }
        let added = Self::of(chunks, key, columns, false)?;
        let mut all = self.0.clone();
        Arc::make_mut(&mut all).extend(added.0.iter().cloned());
        Ok(Self(all))
    }
}
