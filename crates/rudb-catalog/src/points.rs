//! Where the row of each key is, for a statement that reads one row by its key, `engine-v4/13-the-point-path.md`.
//!
//! A table builds this for a key the first time a lookup asks for it, from the key's columns
//! alone, and keeps it for the revision of the rows it was built from. Anything that changes the
//! rows draws a new revision, and the next lookup builds it again. An entry is a hint all the
//! same: the row it names is read with the key's columns and its key compared with the one asked
//! for, so an entry that went stale some way the revision did not catch costs a build and never
//! answers with the wrong row.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::Chunk;

use crate::keys::{Encoded, Key, encode, push};
use crate::table::Rows;

/// A hasher for keys that are already spread: an integer key or the bytes of an encoded one,
/// folded a word at a time. The default hasher is built to resist keys chosen to collide, and on a
/// lookup that was most of the probe.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Fold(u64);

/// An odd constant with a well spread bit pattern, which is all the multiply asks of it.
const ODD: u64 = 0x517c_c1b7_2722_0a95;

impl Fold {
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(ODD);
    }
}

impl Hasher for Fold {
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
    }

    fn write_u64(&mut self, word: u64) {
        self.mix(word);
    }

    fn write_i64(&mut self, word: i64) {
        self.mix(word.cast_unsigned());
    }

    fn write_usize(&mut self, word: usize) {
        self.mix(word as u64);
    }

    /// The multiply leaves what it mixed in the high bits, and the table buckets on the low ones.
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29) ^ (self.0 >> 47)
    }
}

type Map<K> = HashMap<K, u64, BuildHasherDefault<Fold>>;

/// The row number of every key of one key of a table, at one revision of its rows.
#[derive(Debug)]
pub(crate) struct Located {
    /// The revision of the rows this was built from.
    revision: u64,
    /// The key's columns, by place, in the order its keys are encoded in.
    columns: Vec<usize>,
    /// The keys of one `INTEGER` or `BIGINT` column.
    ints: Map<i64>,
    /// Every other key, encoded.
    bytes: Map<Box<[u8]>>,
    /// The number of the first row of each part, and after them the number of rows.
    starts: Vec<u64>,
}

impl Located {
    /// Reads the key's columns of every part of `rows` and notes where each key is.
    fn build(rows: &Rows, columns: &[usize], revision: u64) -> Result<Self> {
        let projected = Key { columns: (0..columns.len()).collect(), primary: false };
        let parts = rows.chunk_count();
        let mut located = Self {
            revision,
            columns: columns.to_vec(),
            ints: Map::default(),
            bytes: Map::default(),
            starts: Vec::with_capacity(parts + 1),
        };
        let mut scratch = Vec::new();
        let mut number = 0_u64;
        for part in 0..parts {
            located.starts.push(number);
            let chunk = rows.read(part, columns)?;
            for row in 0..chunk.len() {
                match encode(&chunk, &projected, row, &mut scratch)? {
                    Encoded::Null => {}
                    Encoded::Int(key) => {
                        located.ints.insert(key, number);
                    }
                    Encoded::Bytes => {
                        located.bytes.insert(scratch.as_slice().into(), number);
                    }
                }
                number += 1;
            }
        }
        located.starts.push(number);
        Ok(located)
    }

    /// The part and the place in it of the row holding `key`, if one does.
    fn find(&self, key: Encoded, scratch: &[u8]) -> Option<(usize, u32)> {
        let number = match key {
            Encoded::Null => return None,
            Encoded::Int(key) => *self.ints.get(&key)?,
            Encoded::Bytes => *self.bytes.get(scratch)?,
        };
        // The last part starting at or before the row. A part can be empty, and then the next one
        // starts where it does, so it is the last such part and not the first that holds the row.
        let part = self.starts.partition_point(|&start| start <= number).checked_sub(1)?;
        let place = u32::try_from(number - self.starts[part]).ok()?;
        Some((part, place))
    }
}

/// What a table keeps of [`Located`], shared by the copies of the table a transaction takes and
/// built under its lock so two lookups at once build it once.
#[derive(Debug, Default)]
pub(crate) struct Points(Mutex<Option<Arc<Located>>>);

impl Clone for Points {
    /// A copy shares what is built. It is for one revision of the rows and a copy that changes
    /// them draws another one.
    fn clone(&self) -> Self {
        Self(Mutex::new(self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()))
    }
}

/// What a lookup by key found.
#[derive(Debug)]
pub enum Point {
    /// No row holds the key.
    Absent,
    /// The row that does, as one row of the columns asked for.
    Found(Chunk),
}

/// Whether a value can be looked for in a key column of type `ty` as it is: a value of the type, or
/// one of the two integer types the keys encode alike. Anything else needs the cast the plan would
/// make, and a lookup that encoded it as it came would miss a row the plan finds.
#[must_use]
pub fn looks_up(value: &Value, ty: &LogicalType) -> bool {
    match value {
        Value::Null => false,
        Value::Integer(_) | Value::BigInt(_) => {
            matches!(ty, LogicalType::Integer | LogicalType::BigInt)
        }
        // A float key has two zeros and any number of NaNs, which the plan compares its own way.
        Value::Float(_) | Value::Double(_) => false,
        value => value.is_of(ty),
    }
}

/// The encoding of a key given as its values, in the order of the columns of [`Located`], the way
/// `encode` writes the key of a row.
fn encoded(values: &[Value], out: &mut Vec<u8>) -> Encoded {
    out.clear();
    if let [value] = values {
        match value {
            Value::Integer(v) => return Encoded::Int(i64::from(*v)),
            Value::BigInt(v) => return Encoded::Int(*v),
            _ => {}
        }
    }
    for value in values {
        if !push(value, out) {
            return Encoded::Null;
        }
    }
    Encoded::Bytes
}

impl Points {
    /// The row of `rows` whose key over `key` is `values`, with the columns `columns`.
    ///
    /// The caller has made sure `key` is a key of the table and that every value passes
    /// [`looks_up`] against its column. `revision` is the table's.
    pub(crate) fn find(
        &self,
        rows: &Rows,
        revision: u64,
        key: &[usize],
        values: &[Value],
        columns: &[usize],
    ) -> Result<Point> {
        let mut scratch = Vec::new();
        let wanted = encoded(values, &mut scratch);
        let mut fresh = false;
        loop {
            let located = self.located(rows, revision, key, fresh)?;
            let Some((part, place)) = located.find(wanted, &scratch) else {
                return Ok(Point::Absent);
            };
            if rows.chunk_len(part)? <= place as usize {
                if fresh {
                    return Err(Error::internal("a key was noted past the end of its part"));
                }
                fresh = true;
                continue;
            }
            let mut read = columns.to_vec();
            read.extend_from_slice(key);
            let chunk = rows.read_selected(part, &read, &[place])?;
            let mut held = Vec::new();
            let at = Key { columns: (columns.len()..read.len()).collect(), primary: false };
            let same = match (encode(&chunk, &at, 0, &mut held)?, wanted) {
                (Encoded::Int(left), Encoded::Int(right)) => left == right,
                (Encoded::Bytes, Encoded::Bytes) => held == scratch,
                _ => false,
            };
            if same {
                let mut kept = chunk.into_columns();
                kept.truncate(columns.len());
                return Ok(Point::Found(Chunk::with_rows(kept, 1)?));
            }
            if fresh {
                return Err(Error::internal("a key found where it was just noted is not there"));
            }
            fresh = true;
        }
    }

    /// The rows' keys over `key` at `revision`, built here when what is held is for anything else
    /// or when `again` says it was wrong.
    fn located(
        &self,
        rows: &Rows,
        revision: u64,
        key: &[usize],
        again: bool,
    ) -> Result<Arc<Located>> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !again
            && let Some(located) = held.as_ref()
            && located.revision == revision
            && located.columns == key
            && located.starts.last() == Some(&(rows.len() as u64))
        {
            return Ok(Arc::clone(located));
        }
        let located = Arc::new(Located::build(rows, key, revision)?);
        *held = Some(Arc::clone(&located));
        Ok(located)
    }
}
