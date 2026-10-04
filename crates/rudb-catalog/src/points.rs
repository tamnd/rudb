//! Where the row of each key is, for a statement that reads one row by its key, `engine-v4/13-the-point-path.md`.
//!
//! A table builds this for a key the first time a lookup asks for it, from the key's columns
//! alone, and keeps it for the placing of the rows it was built from. Anything that moves a row or
//! a key draws a new placing, and the next lookup builds it again. An update that writes other
//! columns of a row where it is keeps the placing, and so does an append, which notes the keys of
//! the rows it adds, so a table written by key is not indexed again for every write. An entry is a hint all the same: the row it names is read with the key's
//! columns and its key compared with the one asked for, so an entry that went stale some way the
//! placing did not catch costs a build and never answers with the wrong row.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

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

/// The row number of every key of one key of a table, at one placing of its rows.
#[derive(Debug)]
pub(crate) struct Located {
    /// The placing of the rows this was built from.
    placed: u64,
    /// The key's columns, by place, in the order its keys are encoded in.
    columns: Vec<usize>,
    /// The keys of one `INTEGER` or `BIGINT` column.
    ints: Map<i64>,
    /// Every other key, encoded.
    bytes: Map<Box<[u8]>>,
    /// The number of the first row of each part, and after them the number of rows.
    starts: Vec<u64>,
    /// The keys in order, each with its row's number, built the first time a read of a range of
    /// keys asks.
    sorted: OnceLock<Sorted>,
}

/// The keys of one column in order, each with its row's number.
#[derive(Debug)]
enum Sorted {
    /// The keys of an `INTEGER` or `BIGINT` column.
    Ints(Vec<(i64, u64)>),
    /// The keys of a `VARCHAR` column as their bytes, which is the order the plan compares text in.
    Text(Vec<(Box<[u8]>, u64)>),
}

/// The bytes of a text key from its encoding: a tag and a length of eight bytes, then the bytes.
fn text_of(encoded: &[u8]) -> Option<&[u8]> {
    match encoded.split_first() {
        Some((b's', rest)) => rest.get(8..),
        _ => None,
    }
}

/// The bound of a read of a range of keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge<'a> {
    Int(i64),
    Text(&'a str),
}

/// The key a row read by a range has to hold, to make sure it is the row the key was noted at.
#[derive(Debug, Clone, Copy)]
enum Wanted<'a> {
    Int(i64),
    Text(&'a [u8]),
}

impl Located {
    /// Reads the key's columns of every part of `rows` and notes where each key is.
    fn build(rows: &Rows, columns: &[usize], placed: u64) -> Result<Self> {
        let projected = Key { columns: (0..columns.len()).collect(), primary: false };
        let parts = rows.chunk_count();
        let mut located = Self {
            placed,
            columns: columns.to_vec(),
            ints: Map::default(),
            bytes: Map::default(),
            starts: Vec::with_capacity(parts + 1),
            sorted: OnceLock::new(),
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

    /// Notes `keys`, the keys of the rows just appended after the first `before`, and works out
    /// again where the parts from `from` on start. An append changes no part before the last one
    /// it found, and keeps every row's number, which is its place among the rows.
    ///
    /// Says whether the rows came out as many as this now counts. When they do not, the caller
    /// drops this and the next lookup builds it again.
    fn extend(&mut self, rows: &Rows, before: u64, from: usize, keys: Vec<Noted>) -> Result<bool> {
        if self.starts.last() != Some(&before) || from >= self.starts.len() {
            return Ok(false);
        }
        self.starts.truncate(from + 1);
        let mut number = self.starts[from];
        for part in from..rows.chunk_count() {
            number += rows.chunk_len(part)? as u64;
            self.starts.push(number);
        }
        if number != before + keys.len() as u64 || number != rows.len() as u64 {
            return Ok(false);
        }
        // The keys in order stay in order for keys that come after every key there is, which is
        // how a table keyed by a counter grows, and are dropped for anything else.
        let mut sorted = self.sorted.get_mut();
        for (key, number) in keys.into_iter().zip(before..) {
            match key {
                Noted::Null => {}
                Noted::Int(key) => {
                    self.ints.insert(key, number);
                    match sorted.as_deref_mut() {
                        Some(Sorted::Ints(held))
                            if held.last().is_none_or(|&(last, _)| last < key) =>
                        {
                            held.push((key, number));
                        }
                        Some(_) => sorted = None,
                        None => {}
                    }
                }
                Noted::Bytes(key) => {
                    match (sorted.as_deref_mut(), text_of(&key)) {
                        (Some(Sorted::Text(held)), Some(text))
                            if held.last().is_none_or(|(last, _)| **last < *text) =>
                        {
                            held.push((text.into(), number));
                        }
                        (Some(_), _) => sorted = None,
                        (None, _) => {}
                    }
                    self.bytes.insert(key, number);
                }
            }
        }
        if sorted.is_none() {
            self.sorted = OnceLock::new();
        }
        Ok(true)
    }

    /// The keys in order with their rows' numbers: the integer ones when there are any, and
    /// otherwise the text ones. A key of one column holds keys of only one of the two.
    fn sorted(&self) -> &Sorted {
        self.sorted.get_or_init(|| {
            if self.bytes.is_empty() {
                let mut sorted: Vec<(i64, u64)> =
                    self.ints.iter().map(|(&key, &number)| (key, number)).collect();
                sorted.sort_unstable();
                return Sorted::Ints(sorted);
            }
            let mut sorted: Vec<(Box<[u8]>, u64)> = self
                .bytes
                .iter()
                .filter_map(|(key, &number)| Some((text_of(key)?.into(), number)))
                .collect();
            sorted.sort_unstable();
            Sorted::Text(sorted)
        })
    }

    /// Whether this was built for `key` of `rows` at `placed`.
    fn fits(&self, rows: &Rows, placed: u64, key: &[usize]) -> bool {
        self.placed == placed
            && self.columns == key
            && self.starts.last() == Some(&(rows.len() as u64))
    }

    /// The part and the place in it of the row holding `key`, and its number, if one does.
    fn find(&self, key: Encoded, scratch: &[u8]) -> Option<(usize, u32, u64)> {
        let number = match key {
            Encoded::Null => return None,
            Encoded::Int(key) => *self.ints.get(&key)?,
            Encoded::Bytes => *self.bytes.get(scratch)?,
        };
        let (part, place) = self.place(number)?;
        Some((part, place, number))
    }

    /// The part and the place in it of the row numbered `number`.
    fn place(&self, number: u64) -> Option<(usize, u32)> {
        // The last part starting at or before the row. A part can be empty, and then the next one
        // starts where it does, so it is the last such part and not the first that holds the row.
        let part = self.starts.partition_point(|&start| start <= number).checked_sub(1)?;
        let place = u32::try_from(number - self.starts[part]).ok()?;
        Some((part, place))
    }
}

/// How many of a table's keys and unique indexes a lookup finds without a lock, by their place
/// among them. A key past these goes through the lock every time.
const HELD: usize = 4;

/// What a table keeps of [`Located`], one for each key a lookup asked for, shared by the copies of
/// the table a transaction takes.
///
/// A lookup reads `built` with a load and writes nothing, so readers on many cores do not pass a
/// cache line between them, `engine-v4/13-the-point-path.md` section 13.5. Each is set once for the
/// rows the table has, and the table drops them all with a new [`Points`] whenever it is about to
/// change its rows. Should one turn out wrong all the same, for rows that changed some way that
/// did not drop it, `stale` says so and the lookups after build into `again` under its lock, which
/// is slower and still right.
#[derive(Debug, Default)]
pub(crate) struct Points {
    built: [OnceLock<Arc<Located>>; HELD],
    stale: AtomicBool,
    again: Mutex<Vec<Arc<Located>>>,
}

impl Clone for Points {
    /// A copy shares what is built. It is for one revision of the rows and a copy that changes
    /// them draws another one.
    fn clone(&self) -> Self {
        let built = std::array::from_fn(|at| {
            let built = OnceLock::new();
            if let Some(located) = self.built[at].get() {
                let _ = built.set(Arc::clone(located));
            }
            built
        });
        Self {
            built,
            stale: AtomicBool::new(self.stale.load(Ordering::Relaxed)),
            again: Mutex::default(),
        }
    }
}

/// The key of one row about to be appended, the way [`Located`] holds it.
#[derive(Debug)]
enum Noted {
    Null,
    Int(i64),
    Bytes(Box<[u8]>),
}

/// The entries of `sorted` on the `reach` side of the bound `order` compares each with.
fn window<T>(sorted: &[T], reach: Reach, order: impl Fn(&T) -> std::cmp::Ordering) -> &[T] {
    let low = match reach {
        Reach::AtLeast => sorted.partition_point(|at| order(at).is_lt()),
        Reach::Above => sorted.partition_point(|at| order(at).is_le()),
        Reach::AtMost | Reach::Below => 0,
    };
    let high = match reach {
        Reach::AtMost => sorted.partition_point(|at| order(at).is_le()),
        Reach::Below => sorted.partition_point(|at| order(at).is_lt()),
        Reach::AtLeast | Reach::Above => sorted.len(),
    };
    sorted.get(low..high).unwrap_or(&[])
}

/// The first `limit` entries of `within`, from the end when `descending`.
fn pick<'a, T, P>(
    within: &'a [T],
    descending: bool,
    limit: usize,
    f: impl Fn(&'a T) -> P,
) -> Vec<P> {
    if descending {
        within.iter().rev().take(limit).map(f).collect()
    } else {
        within.iter().take(limit).map(f).collect()
    }
}

/// The keys of rows about to be appended, for each key a lookup has built where the rows are, which
/// [`Points::appended`] notes once the rows are in.
#[derive(Debug, Default)]
pub(crate) struct Appending(Vec<(usize, Vec<Noted>)>);

/// Which side of a value a read of a range of keys takes, as the key column stands on the left of
/// the comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// `key >= value`.
    AtLeast,
    /// `key > value`.
    Above,
    /// `key <= value`.
    AtMost,
    /// `key < value`.
    Below,
}

/// Where a lookup by key found its row: the part and the place in it, and its number in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    /// The part the row is in.
    pub part: usize,
    /// Its place in the part.
    pub place: u32,
    /// Its number among the rows of the table.
    pub number: u64,
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
    /// The row of `rows` whose key over `key` is `values`, with the columns `columns`, and where it
    /// is, or `None` when no row holds the key.
    ///
    /// The caller has made sure `key` is a key of the table, the one at `which` among its keys and
    /// unique indexes, and that every value passes [`looks_up`] against its column. `placed` is
    /// the table's placing of its rows.
    pub(crate) fn seek(
        &self,
        which: usize,
        rows: &Rows,
        placed: u64,
        key: &[usize],
        values: &[Value],
        columns: &[usize],
    ) -> Result<Option<(Spot, Chunk)>> {
        let mut scratch = Vec::new();
        let wanted = encoded(values, &mut scratch);
        let mut fresh = false;
        loop {
            let again;
            let built = self.built.get(which).and_then(OnceLock::get);
            let located = match built {
                Some(built)
                    if !fresh
                        && !self.stale.load(Ordering::Relaxed)
                        && built.fits(rows, placed, key) =>
                {
                    built.as_ref()
                }
                _ => {
                    again = self.again(which, rows, placed, key, fresh)?;
                    again.as_ref()
                }
            };
            let Some((part, place, number)) = located.find(wanted, &scratch) else {
                return Ok(None);
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
                let spot = Spot { part, place, number };
                return Ok(Some((spot, Chunk::with_rows(kept, 1)?)));
            }
            if fresh {
                return Err(Error::internal("a key found where it was just noted is not there"));
            }
            fresh = true;
        }
    }

    /// The keys of the rows of `chunks`, for each key whose rows are built, ahead of appending
    /// them. Nothing when nothing is built, which is a table nobody has looked a row up in since it
    /// last changed some other way.
    pub(crate) fn appending(&mut self, chunks: &[Chunk]) -> Result<Appending> {
        if *self.stale.get_mut() {
            return Ok(Appending::default());
        }
        let mut appending = Vec::new();
        let mut scratch = Vec::new();
        for (which, slot) in self.built.iter_mut().enumerate() {
            let Some(located) = slot.get_mut() else { continue };
            let key = Key { columns: located.columns.clone(), primary: false };
            let mut keys = Vec::with_capacity(chunks.iter().map(Chunk::len).sum());
            for chunk in chunks {
                for row in 0..chunk.len() {
                    keys.push(match encode(chunk, &key, row, &mut scratch)? {
                        Encoded::Null => Noted::Null,
                        Encoded::Int(key) => Noted::Int(key),
                        Encoded::Bytes => Noted::Bytes(scratch.as_slice().into()),
                    });
                }
            }
            appending.push((which, keys));
        }
        Ok(Appending(appending))
    }

    /// Notes the keys of the rows just appended, which [`Self::appending`] read before the append,
    /// into what is built, for a table that keeps its placing across the append. `before` is how
    /// many rows there were and `from` the first part the append can have changed.
    ///
    /// What is built for a key that copies of the table share, because a transaction holds one, is
    /// dropped rather than written, and so is anything that does not come out right.
    pub(crate) fn appended(&mut self, appending: Appending, rows: &Rows, before: u64, from: usize) {
        self.again.get_mut().unwrap_or_else(PoisonError::into_inner).clear();
        let mut appending = appending.0.into_iter().peekable();
        for (which, slot) in self.built.iter_mut().enumerate() {
            let keys = appending.next_if(|(at, _)| *at == which).map(|(_, keys)| keys);
            let kept = match (slot.get_mut().and_then(Arc::get_mut), keys) {
                (Some(located), Some(keys)) => {
                    located.extend(rows, before, from, keys).unwrap_or(false)
                }
                _ => false,
            };
            if !kept {
                *slot = OnceLock::new();
            }
        }
        // Whatever a lookup found wrong is gone with what it was found in.
        if self.built.iter().all(|slot| slot.get().is_none()) {
            *self.stale.get_mut() = false;
        }
    }

    /// The first `limit` rows of `rows` in the order of the key over the one column `key` whose key
    /// is on the `reach` side of `bound`, the highest first when `descending`, with the columns
    /// `columns`, one row to a chunk.
    ///
    /// The caller has made sure `key` is the key at `which`, as for [`Self::seek`], and that its
    /// column is an `INTEGER` or a `BIGINT` for an integer bound and a `VARCHAR` for a text one. A
    /// null key is on no side of any value.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn range(
        &self,
        which: usize,
        rows: &Rows,
        placed: u64,
        key: usize,
        (reach, bound): (Reach, Edge<'_>),
        descending: bool,
        limit: usize,
        columns: &[usize],
    ) -> Result<Vec<Chunk>> {
        let held = [key];
        let mut read = columns.to_vec();
        read.push(key);
        let mut fresh = false;
        'again: loop {
            let again;
            let built = self.built.get(which).and_then(OnceLock::get);
            let located = match built {
                Some(built)
                    if !fresh
                        && !self.stale.load(Ordering::Relaxed)
                        && built.fits(rows, placed, &held) =>
                {
                    built.as_ref()
                }
                _ => {
                    again = self.again(which, rows, placed, &held, fresh)?;
                    again.as_ref()
                }
            };
            let picked: Vec<(Wanted<'_>, u64)> = match (located.sorted(), bound) {
                (Sorted::Ints(sorted), Edge::Int(bound)) => {
                    let within = window(sorted, reach, |(at, _)| at.cmp(&bound));
                    pick(within, descending, limit, |&(at, number)| (Wanted::Int(at), number))
                }
                (Sorted::Text(sorted), Edge::Text(bound)) => {
                    let within = window(sorted, reach, |(at, _)| (**at).cmp(bound.as_bytes()));
                    pick(within, descending, limit, |(at, number)| (Wanted::Text(at), *number))
                }
                // Keys of the other kind, so none the bound reaches, which happens only for a
                // table with no rows.
                _ => Vec::new(),
            };
            let mut chunks = Vec::with_capacity(picked.len());
            for (wanted, number) in picked {
                let found = match located.place(number) {
                    Some((part, place)) if (place as usize) < rows.chunk_len(part)? => {
                        let chunk = rows.read_selected(part, &read, &[place])?;
                        let same = match (chunk.column(columns.len())?.value_at(0), wanted) {
                            (Value::Integer(at), Wanted::Int(wanted)) => i64::from(at) == wanted,
                            (Value::BigInt(at), Wanted::Int(wanted)) => at == wanted,
                            (Value::Varchar(at), Wanted::Text(wanted)) => at.as_bytes() == wanted,
                            _ => false,
                        };
                        same.then_some(chunk)
                    }
                    _ => None,
                };
                let Some(chunk) = found else {
                    if fresh {
                        return Err(Error::internal(
                            "a key found where it was just noted is not there",
                        ));
                    }
                    fresh = true;
                    continue 'again;
                };
                let mut kept = chunk.into_columns();
                kept.truncate(columns.len());
                chunks.push(Chunk::with_rows(kept, 1)?);
            }
            return Ok(chunks);
        }
    }

    /// The rows' keys over `key` at `placed` when `built` cannot answer: built into `built` the
    /// first time, and otherwise into `again` when what that holds is for anything else or when
    /// `fresh` says what was used was wrong.
    fn again(
        &self,
        which: usize,
        rows: &Rows,
        placed: u64,
        key: &[usize],
        fresh: bool,
    ) -> Result<Arc<Located>> {
        let mut held = self.again.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = self.built.get(which);
        if fresh {
            if slot.and_then(OnceLock::get).is_some() {
                self.stale.store(true, Ordering::Relaxed);
            }
        } else if let Some(slot) = slot
            && slot.get().is_none()
        {
            // Under the lock, so two lookups building at once build once.
            let located = Arc::new(Located::build(rows, key, placed)?);
            return Ok(Arc::clone(slot.get_or_init(|| located)));
        } else if let Some(located) = held.iter().find(|held| held.fits(rows, placed, key)) {
            return Ok(Arc::clone(located));
        }
        let located = Arc::new(Located::build(rows, key, placed)?);
        held.retain(|held| held.columns != key);
        held.push(Arc::clone(&located));
        Ok(located)
    }
}
