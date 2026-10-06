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
use rudb_index::normalize;
use rudb_vector::Chunk;

use crate::keys::{Encoded, Key, encode, push};
use crate::sealed::Sealed;
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
///
/// The keys are held in levels, each behind an [`Arc`], so that the copy of a table a transaction
/// takes shares them. An append notes its keys in the last level when nothing else holds it and in
/// a level of its own when something does. It used to be one map, and the first append of every
/// transaction found it shared with the committed table and dropped it, so the next lookup read
/// the key of every row again to build it: a YCSB load in transactions of a thousand rows built
/// it ten thousand times over ten million rows. A level is folded into the one before it whenever
/// it grows to half that one's size, so a lookup looks through no more levels than the logarithm
/// of the keys, and [`Self::settle`] puts them back into one once nothing else holds the first.
#[derive(Debug)]
pub(crate) struct Located {
    /// The placing of the rows this was built from.
    placed: u64,
    /// The key's columns, by place, in the order its keys are encoded in.
    columns: Vec<usize>,
    /// The keys, oldest first. Never empty.
    levels: Vec<Arc<Level>>,
    /// The keys of the first rows of the file, out of the run a checkpoint sealed them into, older
    /// than every level. A key here is numbered by the place of its row in the file, which is the
    /// number it has when no row was taken out, and the rows gone from the file are in `taken`.
    sealed: Option<Arc<Sealed>>,
    /// The number of the first row of each part, and after them the number of rows.
    starts: Vec<u64>,
    /// The numbers the keys have for the rows taken out since this was built, in order. A key
    /// whose number is here has no row, and any other key's row is numbered as the key has it
    /// less how many of these come before it, see [`Self::take`].
    taken: Vec<u64>,
    /// The keys in order, each with its row's number, built the first time a read of a range of
    /// keys asks.
    sorted: OnceLock<Ordered>,
    /// Whether this is for a key or a unique index. One for an index that is not unique holds no
    /// levels and nothing in `sorted`, only `normal`, since a key there can name more than one row.
    unique: bool,
    /// The keys normalized, built the first time a read of a range over the front of the key asks,
    /// and `None` inside when a column of the key is of a type the normalized form does not write.
    normal: OnceLock<Option<Normal>>,
}

/// The keys of [`Located`] as `rudb_index::normalize` writes them, each with its row's number, in
/// runs the way [`Ordered`] holds them, oldest first. A run is in the order of the key and then of
/// the number, and holds a key once for each row that holds it, so this serves an index that is
/// not unique as well as a key.
///
/// The normalized form sorts as the plan compares the key's columns one after another, and the
/// form of the first columns of a key is the front of the form of the whole key. So the rows whose
/// first columns hold given values are one window of each run, in the order of the columns after
/// them, which is what a read of a range over the front of a key reads.
#[derive(Debug, Clone, Default)]
struct Normal(Vec<Arc<Vec<NormalKey>>>);

/// A key as `rudb_index::normalize` writes it, with its row's number.
type NormalKey = (Box<[u8]>, u64);

impl Normal {
    /// Adds `keys` as a run, and merges a run into the one before it while it holds at least half
    /// as many keys, as [`Ordered::note`] does.
    fn note(&mut self, mut keys: Vec<(Box<[u8]>, u64)>) {
        if keys.is_empty() {
            return;
        }
        keys.sort_unstable();
        match self.0.last_mut().and_then(Arc::get_mut) {
            Some(last) if last.last().is_none_or(|held| *held < keys[0]) => last.extend(keys),
            _ => self.0.push(Arc::new(keys)),
        }
        while let [.., before, last] = self.0.as_slice()
            && before.len() <= 2 * last.len()
        {
            let last = self.0.pop().expect("two runs");
            let before = self.0.last_mut().expect("two runs");
            *before = Arc::new(interleave(before, &last));
        }
    }
}

/// Two runs in order as one, keeping every entry of both.
fn interleave<T: Ord + Clone>(older: &[T], newer: &[T]) -> Vec<T> {
    let mut merged = Vec::with_capacity(older.len() + newer.len());
    let (mut old, mut new) = (0, 0);
    while let (Some(left), Some(right)) = (older.get(old), newer.get(new)) {
        if left <= right {
            merged.push(left.clone());
            old += 1;
        } else {
            merged.push(right.clone());
            new += 1;
        }
    }
    merged.extend_from_slice(&older[old..]);
    merged.extend_from_slice(&newer[new..]);
    merged
}

/// The keys of [`Located`] in order, in runs, oldest first, each behind an [`Arc`] the way the
/// levels are. A key in more than one run is the newest run's.
///
/// A copy of a table shares the runs and an append adds its keys as a run of its own, sorted on
/// their own, and a run is merged into the one before it whenever it grows to half that one's
/// size. Before, a copy left the keys in order behind, and a read of a range after any write to a
/// table of ten million keys sorted all ten million again.
#[derive(Debug, Clone)]
struct Ordered(Vec<Arc<Sorted>>);

/// One level of the keys of [`Located`].
#[derive(Debug, Clone, Default)]
struct Level {
    /// The keys of one `INTEGER` or `BIGINT` column.
    ints: Map<i64>,
    /// Every other key, encoded.
    bytes: Map<Box<[u8]>>,
}

impl Level {
    fn len(&self) -> usize {
        self.ints.len() + self.bytes.len()
    }

    fn find(&self, key: Encoded, scratch: &[u8]) -> Option<u64> {
        match key {
            Encoded::Null => None,
            Encoded::Int(key) => self.ints.get(&key).copied(),
            Encoded::Bytes => self.bytes.get(scratch).copied(),
        }
    }

    /// Takes in the keys of `newer`, moving them when nothing else holds it. A key both hold is
    /// `newer`'s.
    fn take(&mut self, newer: Arc<Self>) {
        match Arc::try_unwrap(newer) {
            Ok(newer) => {
                self.ints.extend(newer.ints);
                self.bytes.extend(newer.bytes);
            }
            Err(newer) => {
                self.ints.extend(newer.ints.iter().map(|(&key, &number)| (key, number)));
                self.bytes.extend(newer.bytes.iter().map(|(key, &number)| (key.clone(), number)));
            }
        }
    }
}

/// The keys of one column in order, each with its row's number.
#[derive(Debug, Clone)]
enum Sorted {
    /// The keys of an `INTEGER` or `BIGINT` column.
    Ints(Vec<(i64, u64)>),
    /// The keys of a `VARCHAR` column as their bytes, which is the order the plan compares text in.
    Text(Vec<(Box<[u8]>, u64)>),
}

impl Sorted {
    fn len(&self) -> usize {
        match self {
            Self::Ints(keys) => keys.len(),
            Self::Text(keys) => keys.len(),
        }
    }

    /// Adds `run` at the end when every key of it comes after every key here, which is how a
    /// table keyed by a counter grows, and hands it back otherwise.
    fn extend_after(&mut self, run: Self) -> std::result::Result<(), Self> {
        match (self, run) {
            (Self::Ints(held), Self::Ints(run))
                if held.last().zip(run.first()).is_none_or(|(last, first)| last.0 < first.0) =>
            {
                held.extend(run);
                Ok(())
            }
            (Self::Text(held), Self::Text(run))
                if held.last().zip(run.first()).is_none_or(|(last, first)| last.0 < first.0) =>
            {
                held.extend(run);
                Ok(())
            }
            (_, run) => Err(run),
        }
    }

    /// The keys of this and of `newer` in one run, with `newer`'s number for a key both hold.
    fn merged(&self, newer: &Self) -> Self {
        match (self, newer) {
            (Self::Ints(older), Self::Ints(newer)) => Self::Ints(merge(older, newer)),
            (Self::Text(older), Self::Text(newer)) => Self::Text(merge(older, newer)),
            // [`Ordered::note`] never puts keys of the other kind beside these.
            _ => newer.clone(),
        }
    }
}

/// Two runs of keys in order as one, with `newer`'s number for a key both hold.
fn merge<K: Ord + Clone>(older: &[(K, u64)], newer: &[(K, u64)]) -> Vec<(K, u64)> {
    let mut merged = Vec::with_capacity(older.len() + newer.len());
    let (mut old, mut new) = (0, 0);
    while let (Some(left), Some(right)) = (older.get(old), newer.get(new)) {
        match left.0.cmp(&right.0) {
            std::cmp::Ordering::Less => {
                merged.push(left.clone());
                old += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right.clone());
                new += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(right.clone());
                old += 1;
                new += 1;
            }
        }
    }
    merged.extend_from_slice(&older[old..]);
    merged.extend_from_slice(&newer[new..]);
    merged
}

/// `keys` in order, with the last number given for a key given more than once.
fn sorted_run<K: Ord>(mut keys: Vec<(K, u64)>) -> Vec<(K, u64)> {
    // Stable, so of equal keys the one given last comes last.
    keys.sort_by(|left, right| left.0.cmp(&right.0));
    keys.dedup_by(|later, kept| {
        let same = later.0 == kept.0;
        if same {
            kept.1 = later.1;
        }
        same
    });
    keys
}

impl Ordered {
    /// Adds the keys an append noted, the integer ones or the text ones, as a run, and merges a
    /// run into the one before it while it holds at least half as many keys. Answers `false` when
    /// the keys are not of the kind the runs hold, and the caller drops the runs.
    fn note(&mut self, ints: Vec<(i64, u64)>, texts: Vec<(Box<[u8]>, u64)>) -> bool {
        let run = match (self.0.first().map(|run| &**run), ints.is_empty(), texts.is_empty()) {
            (_, true, true) => return true,
            (Some(Sorted::Ints(_)), false, true) => Sorted::Ints(sorted_run(ints)),
            (Some(Sorted::Text(_)), true, false) => Sorted::Text(sorted_run(texts)),
            _ => return false,
        };
        let run = match self.0.last_mut().and_then(Arc::get_mut) {
            Some(last) => last.extend_after(run).err(),
            None => Some(run),
        };
        if let Some(run) = run {
            self.0.push(Arc::new(run));
        }
        while let [.., before, last] = self.0.as_slice()
            && before.len() <= 2 * last.len()
        {
            let last = self.0.pop().expect("two runs");
            let before = self.0.last_mut().expect("two runs");
            *before = Arc::new(before.merged(&last));
        }
        true
    }
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
    /// Reads the key's columns of every part of `rows` and notes where each key is. For an index
    /// that is not `unique` only where the parts start, and the keys wait for a read of a range.
    fn build(rows: &Rows, columns: &[usize], placed: u64, unique: bool) -> Result<Self> {
        let projected = Key { columns: (0..columns.len()).collect(), primary: false };
        let parts = rows.chunk_count();
        let mut level = Level::default();
        let mut starts = Vec::with_capacity(parts + 1);
        let mut scratch = Vec::new();
        let mut number = 0_u64;
        for part in 0..parts {
            starts.push(number);
            if !unique {
                number += rows.chunk_len(part)? as u64;
                continue;
            }
            let chunk = rows.read(part, columns)?;
            for row in 0..chunk.len() {
                match encode(&chunk, &projected, row, &mut scratch)? {
                    Encoded::Null => {}
                    Encoded::Int(key) => {
                        level.ints.insert(key, number);
                    }
                    Encoded::Bytes => {
                        level.bytes.insert(scratch.as_slice().into(), number);
                    }
                }
                number += 1;
            }
        }
        starts.push(number);
        Ok(Self {
            placed,
            columns: columns.to_vec(),
            levels: vec![Arc::new(level)],
            sealed: None,
            starts,
            taken: Vec::new(),
            sorted: OnceLock::new(),
            unique,
            normal: OnceLock::new(),
        })
    }

    /// The keys of `rows` out of the run a checkpoint sealed them into, when the rows are a file
    /// that has one for the key, and the keys of the rows the run does not cover read the way
    /// [`Self::build`] reads them. `None` when there is no run, or when the rows gone from the
    /// file are a quarter of it or a key column was written over, and then the caller builds.
    fn opened(rows: &Rows, columns: &[usize], placed: u64) -> Result<Option<Self>> {
        let Some((reader, gone)) = rows.file() else { return Ok(None) };
        if gone.is_some_and(|gone| columns.iter().any(|&column| gone.touched(column))) {
            return Ok(None);
        }
        let Some(sealed) = Sealed::read(reader, columns) else { return Ok(None) };
        let projected = Key { columns: (0..columns.len()).collect(), primary: false };
        let mut level = Level::default();
        let mut starts = Vec::with_capacity(rows.chunk_count() + 1);
        let mut taken = Vec::new();
        let mut scratch = Vec::new();
        // The number of the next row, and the place in the file of the next one there, gone or not.
        let (mut number, mut place) = (0_u64, 0_u64);
        let mut note = |chunk: &Chunk, row: usize, place: u64| -> Result<()> {
            match encode(chunk, &projected, row, &mut scratch)? {
                Encoded::Null => {}
                Encoded::Int(key) => {
                    level.ints.insert(key, place);
                }
                Encoded::Bytes => {
                    level.bytes.insert(scratch.as_slice().into(), place);
                }
            }
            Ok(())
        };
        for part in 0..reader.parts() {
            starts.push(number);
            let size = reader.part_rows(part) as u64;
            let lost = gone.map(|gone| gone.slots(part)).unwrap_or_default();
            taken.extend(lost.iter().map(|&slot| place + u64::from(slot)));
            if place >= sealed.covered {
                let chunk = rows.read(part, columns)?;
                let live = gone.and_then(|gone| gone.live(part, size as usize));
                for row in 0..chunk.len() {
                    let at = live.as_ref().map_or(row as u64, |live| u64::from(live[row]));
                    note(&chunk, row, place + at)?;
                }
            } else if place + size > sealed.covered {
                return Ok(None);
            }
            number += size - lost.len() as u64;
            place += size;
        }
        for part in reader.parts()..rows.chunk_count() {
            starts.push(number);
            let chunk = rows.read(part, columns)?;
            for row in 0..chunk.len() {
                note(&chunk, row, place)?;
                place += 1;
            }
            number += chunk.len() as u64;
        }
        starts.push(number);
        if number != rows.len() as u64 || taken.len() as u64 > number / 4 {
            return Ok(None);
        }
        Ok(Some(Self {
            placed,
            columns: columns.to_vec(),
            levels: vec![Arc::new(level)],
            sealed: Some(Arc::new(sealed)),
            starts,
            taken,
            sorted: OnceLock::new(),
            unique: true,
            normal: OnceLock::new(),
        }))
    }

    /// A copy that shares this one's levels and its keys in order, for a table that is about to
    /// note keys of its own while something else holds this.
    fn layered(&self) -> Self {
        Self {
            placed: self.placed,
            columns: self.columns.clone(),
            levels: self.levels.clone(),
            sealed: self.sealed.clone(),
            starts: self.starts.clone(),
            taken: self.taken.clone(),
            sorted: self.sorted.get().cloned().map(OnceLock::from).unwrap_or_default(),
            unique: self.unique,
            normal: self.normal.get().cloned().map(OnceLock::from).unwrap_or_default(),
        }
    }

    /// Folds the last level into the one before it while it holds at least half as many keys,
    /// which keeps each level at least twice the size of the one after it.
    fn fold(&mut self) {
        while let [.., before, last] = self.levels.as_slice()
            && before.len() <= 2 * last.len()
        {
            let last = self.levels.pop().expect("two levels");
            Arc::make_mut(self.levels.last_mut().expect("two levels")).take(last);
        }
    }

    /// Puts every level into the first, when nothing else holds the first, which is when the copy
    /// of the table that shared it is gone.
    fn settle(&mut self) {
        if self.levels.len() < 2 || Arc::get_mut(&mut self.levels[0]).is_none() {
            return;
        }
        let rest = self.levels.split_off(1);
        let first = Arc::get_mut(&mut self.levels[0]).expect("asked just above");
        for level in rest {
            first.take(level);
        }
    }

    fn len(&self) -> usize {
        let sealed = self.sealed.as_ref().map_or(0, |sealed| sealed.len());
        self.levels.iter().map(|level| level.len()).sum::<usize>() + sealed
    }

    /// Notes `keys`, the keys of the rows just appended after the first `before`, and works out
    /// again where the parts from `from` on start. An append changes no part before the last one
    /// it found, and keeps every row's number, which is its place among the rows.
    ///
    /// `normal` has the keys normalized, when the keys are held that way too, see
    /// [`Self::normal`], and when it does not they are dropped and read again when next asked.
    ///
    /// Says whether the rows came out as many as this now counts. When they do not, the caller
    /// drops this and the next lookup builds it again.
    fn extend(
        &mut self,
        rows: &Rows,
        before: u64,
        from: usize,
        keys: Vec<Noted>,
        normal: Option<Vec<Box<[u8]>>>,
    ) -> Result<bool> {
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
        // The keys in order get a run of the keys noted here, and are dropped when a key is not of
        // the kind they hold.
        let mut sorted = self.sorted.get_mut().map(|_| (Vec::new(), Vec::new()));
        // Every row taken out came before these, so their keys skip that many numbers.
        let first = before + self.taken.len() as u64;
        if let Some(Some(held)) = self.normal.get_mut() {
            match normal {
                Some(normal) if normal.len() == keys.len() => {
                    held.note(normal.into_iter().zip(first..).collect());
                }
                _ => self.normal = OnceLock::new(),
            }
        }
        if !self.unique {
            return Ok(true);
        }
        let level = {
            if !self.levels.last_mut().is_some_and(|level| Arc::get_mut(level).is_some()) {
                self.levels.push(Arc::default());
            }
            Arc::get_mut(self.levels.last_mut().expect("just made sure")).expect("just made sure")
        };
        for (key, number) in keys.into_iter().zip(first..) {
            match key {
                Noted::Null => {}
                Noted::Int(key) => {
                    level.ints.insert(key, number);
                    if let Some((ints, _)) = &mut sorted {
                        ints.push((key, number));
                    }
                }
                Noted::Bytes(key) => {
                    match (&mut sorted, text_of(&key)) {
                        (Some((_, texts)), Some(text)) => texts.push((text.into(), number)),
                        (Some(_), None) => sorted = None,
                        (None, _) => {}
                    }
                    level.bytes.insert(key, number);
                }
            }
        }
        self.fold();
        match (sorted, self.sorted.get_mut()) {
            (Some((ints, texts)), Some(ordered)) => {
                if !ordered.note(ints, texts) {
                    self.sorted = OnceLock::new();
                }
            }
            _ => self.sorted = OnceLock::new(),
        }
        Ok(true)
    }

    /// Notes that the rows `numbers` names, which rise, are taken out of `rows`, and works out
    /// again where the parts start: from the parts themselves when `moved` says they did not stay
    /// as they were, and otherwise by moving each start down past the rows taken before it.
    ///
    /// The keys stay where they are and the rows' numbers go into `taken`, so a delete by key
    /// costs the rows it takes and not a read of every key. Says whether the rows came out as many
    /// as this now counts, and `false` as well once a quarter of the keys held are of rows taken
    /// out, and then the caller drops this and the next lookup builds it again.
    fn take(&mut self, rows: &Rows, numbers: &[u64], moved: bool) -> Result<bool> {
        let Some(&before) = self.starts.last() else { return Ok(false) };
        if numbers.windows(2).any(|pair| pair[0] >= pair[1])
            || numbers.last().is_some_and(|&last| last >= before)
        {
            return Ok(false);
        }
        // A row's number as its key has it is its number now plus how many rows taken before
        // came before it.
        let mut taken = Vec::with_capacity(self.taken.len() + numbers.len());
        let mut earlier = self.taken.iter().copied().peekable();
        let mut passed = 0;
        for &number in numbers {
            let mut noted = number + passed;
            while let Some(skipped) = earlier.next_if(|&skipped| skipped <= noted) {
                taken.push(skipped);
                noted += 1;
                passed += 1;
            }
            taken.push(noted);
        }
        taken.extend(earlier);
        let left = before - numbers.len() as u64;
        if left != rows.len() as u64 || taken.len() as u64 > left / 4 {
            return Ok(false);
        }
        if moved || self.starts.len() != rows.chunk_count() + 1 {
            let mut starts = Vec::with_capacity(rows.chunk_count() + 1);
            let mut number = 0;
            for part in 0..rows.chunk_count() {
                starts.push(number);
                number += rows.chunk_len(part)? as u64;
            }
            starts.push(number);
            self.starts = starts;
        } else {
            let mut gone = numbers.iter().peekable();
            let mut down = 0;
            for start in &mut self.starts {
                while gone.next_if(|&&number| number < *start).is_some() {
                    down += 1;
                }
                *start -= down;
            }
        }
        if self.starts.last() != Some(&left) {
            return Ok(false);
        }
        self.taken = taken;
        Ok(true)
    }

    /// The number of the row whose key has the number `noted`, or `None` when it was taken out.
    fn number(&self, noted: u64) -> Option<u64> {
        let before = self.taken.partition_point(|&taken| taken < noted);
        (self.taken.get(before) != Some(&noted)).then(|| noted - before as u64)
    }

    /// Whether the row whose key has the number `noted` is still there.
    fn kept(&self, noted: u64) -> bool {
        self.taken.binary_search(&noted).is_err()
    }

    /// The keys in order with their rows' numbers, sorted from the levels the first time a read
    /// of a range asks.
    fn sorted(&self) -> &Ordered {
        self.sorted.get_or_init(|| Ordered(vec![Arc::new(self.sort())]))
    }

    /// The keys normalized with their rows' numbers, read from `rows` the first time a read of a
    /// range over the front of the key asks, or `None` when a column of the key is of a type the
    /// normalized form does not write.
    fn normal(&self, rows: &Rows) -> Result<Option<&Normal>> {
        if let Some(normal) = self.normal.get() {
            return Ok(normal.as_ref());
        }
        let mut keys = Vec::with_capacity(rows.len());
        // A row's number as its key has it skips the numbers of the rows taken out.
        let mut taken = self.taken.iter().copied().peekable();
        let mut noted = 0_u64;
        let mut values = Vec::with_capacity(self.columns.len());
        let mut key = Vec::new();
        let mut normal = None;
        'read: {
            for part in 0..rows.chunk_count() {
                let chunk = rows.read(part, &self.columns)?;
                for row in 0..chunk.len() {
                    while taken.next_if_eq(&noted).is_some() {
                        noted += 1;
                    }
                    values.clear();
                    for column in 0..self.columns.len() {
                        values.push(chunk.column(column)?.value_at(row));
                    }
                    key.clear();
                    if !normalize(&values, &mut key) {
                        break 'read;
                    }
                    keys.push((key.as_slice().into(), noted));
                    noted += 1;
                }
            }
            let mut held = Normal::default();
            held.note(keys);
            normal = Some(held);
        }
        let _ = self.normal.set(normal);
        Ok(self.normal.get().and_then(Option::as_ref))
    }

    /// The keys of every level in order with their rows' numbers: the integer ones when there are
    /// any, and otherwise the text ones. A key of one column holds keys of only one of the two.
    ///
    /// A key two levels hold is the newer one's, which an append after a placing that kept the
    /// rows never makes and which is kept right all the same.
    fn sort(&self) -> Sorted {
        // Each key with how new its level is, newest first among equal keys, so the first of
        // a run of equal keys is the one kept.
        // The run sealed at checkpoint is older than every level, and the levels count from one.
        let newest = |at: usize| usize::MAX - at;
        let sealed = self.sealed.as_deref();
        if sealed.is_none_or(Sealed::is_ints)
            && self.levels.iter().all(|level| level.bytes.is_empty())
        {
            let mut sorted: Vec<(i64, usize, u64)> = Vec::with_capacity(self.len());
            if let Some(keys) = sealed.and_then(Sealed::ints) {
                sorted.extend(
                    keys.iter()
                        .filter(|&&(_, number)| self.kept(number))
                        .map(|&(key, number)| (key, newest(0), number)),
                );
            }
            for (at, level) in self.levels.iter().enumerate() {
                sorted.extend(
                    level
                        .ints
                        .iter()
                        .filter(|&(_, &number)| self.kept(number))
                        .map(|(&key, &number)| (key, newest(at + 1), number)),
                );
            }
            sorted.sort_unstable();
            sorted.dedup_by_key(|&mut (key, _, _)| key);
            return Sorted::Ints(
                sorted.into_iter().map(|(key, _, number)| (key, number)).collect(),
            );
        }
        let mut sorted: Vec<(Box<[u8]>, usize, u64)> = Vec::with_capacity(self.len());
        if let Some(sealed) = sealed {
            sorted.extend(
                sealed
                    .texts()
                    .into_iter()
                    .filter(|&(_, number)| self.kept(number))
                    .map(|(key, number)| (key, newest(0), number)),
            );
        }
        for (at, level) in self.levels.iter().enumerate() {
            sorted.extend(
                level.bytes.iter().filter(|&(_, &number)| self.kept(number)).filter_map(
                    |(key, &number)| Some((text_of(key)?.into(), newest(at + 1), number)),
                ),
            );
        }
        sorted.sort_unstable();
        sorted.dedup_by(|later, first| later.0 == first.0);
        Sorted::Text(sorted.into_iter().map(|(key, _, number)| (key, number)).collect())
    }

    /// Whether this was built for `key` of `rows` at `placed`, as a key or a unique index when
    /// `unique` says so.
    fn fits(&self, rows: &Rows, placed: u64, key: &[usize], unique: bool) -> bool {
        self.placed == placed
            && self.unique == unique
            && self.columns == key
            && self.starts.last() == Some(&(rows.len() as u64))
    }

    /// The part and the place in it of the row holding `key`, and its number, if one does. `key`
    /// is encoded from `values` into `scratch`, and the run sealed at checkpoint takes the values.
    fn find(&self, key: Encoded, scratch: &[u8], values: &[Value]) -> Option<(usize, u32, u64)> {
        let noted = match self.levels.iter().rev().find_map(|level| level.find(key, scratch)) {
            Some(noted) => noted,
            None => self.sealed.as_ref()?.find(key, values, |noted| self.kept(noted))?,
        };
        let number = self.number(noted)?;
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

/// The keys of `rows` out of a run sealed at checkpoint when there is one, and read from the rows
/// when not. A checkpoint seals only keys and unique indexes.
fn opened_or_built(rows: &Rows, key: &[usize], placed: u64, unique: bool) -> Result<Located> {
    let opened = if unique { Located::opened(rows, key, placed)? } else { None };
    match opened {
        Some(located) => Ok(located),
        None => Located::build(rows, key, placed, unique),
    }
}

/// `located` to change, copied first when something else holds it, sharing its levels.
fn unshared(located: &mut Arc<Located>) -> &mut Located {
    if Arc::get_mut(located).is_none() {
        *located = Arc::new(located.layered());
    }
    Arc::get_mut(located).expect("just made sure")
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

/// The first `limit` entries over `windows`, which are of runs in the order of `key`, oldest run
/// first, from the highest when `descending`, that `f` keeps. A key more than one window holds is
/// taken from the newest, and is passed over when `f` does not keep that one.
fn pick<'a, T, K: Ord + Copy, P>(
    windows: &[&'a [T]],
    descending: bool,
    limit: usize,
    key: impl Fn(&'a T) -> K,
    f: impl Fn(&'a T) -> Option<P>,
) -> Vec<P> {
    let head = |window: &'a [T], taken: usize| {
        if descending {
            window.len().checked_sub(taken + 1).map(|at| &window[at])
        } else {
            window.get(taken)
        }
    };
    let mut picked = Vec::new();
    // How many entries of each window are behind the ones still to pick.
    let mut taken = vec![0; windows.len()];
    while picked.len() < limit {
        let mut best: Option<(&'a T, K)> = None;
        // Oldest first, so the newest of equal keys is the one left as the best.
        for (&window, &taken) in windows.iter().zip(&taken) {
            if let Some(entry) = head(window, taken) {
                let at = key(entry);
                if best.is_none_or(|(_, best)| if descending { at >= best } else { at <= best }) {
                    best = Some((entry, at));
                }
            }
        }
        let Some((entry, best)) = best else { break };
        for (&window, taken) in windows.iter().zip(&mut taken) {
            if head(window, *taken).is_some_and(|entry| key(entry) == best) {
                *taken += 1;
            }
        }
        picked.extend(f(entry));
    }
    picked
}

/// The keys of rows about to be appended, for each key a lookup has built where the rows are, which
/// [`Points::appended`] notes once the rows are in, and the keys normalized for each that holds
/// them that way too.
#[derive(Debug, Default)]
pub(crate) struct Appending(Vec<(usize, Vec<Noted>, Option<Normalized>)>);

/// The keys of rows about to be appended as `rudb_index::normalize` writes them, in the rows'
/// order.
type Normalized = Vec<Box<[u8]>>;

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

/// The normalized keys over `columns` of the rows of `chunks`, or `None` when a value is of a type
/// the normalized form does not write.
fn normalized(chunks: &[Chunk], columns: &[usize]) -> Result<Option<Vec<Box<[u8]>>>> {
    let mut keys = Vec::with_capacity(chunks.iter().map(Chunk::len).sum());
    let mut values = Vec::with_capacity(columns.len());
    let mut key = Vec::new();
    for chunk in chunks {
        for row in 0..chunk.len() {
            values.clear();
            for &column in columns {
                values.push(chunk.column(column)?.value_at(row));
            }
            key.clear();
            if !normalize(&values, &mut key) {
                return Ok(None);
            }
            keys.push(key.as_slice().into());
        }
    }
    Ok(Some(keys))
}

/// The first normalized key a read of a range over the front `front` reads, and the first one past
/// it, or `None` when a value is of a type the normalized form does not write.
///
/// A key holds the front when its form starts with the front's and the next byte, if any, starts
/// a column, which is `0x00` for a null or `0x01` for a value. Only an escaped `0x00` inside text
/// is followed by `0xFF`, so the keys holding the front are the ones from it to it and `0xFF`. A
/// bound on the next column cuts that the same way: the keys whose next column equals the bound
/// run from the front and the bound to those and `0xFF`, and a value of the next column starts at
/// the front and `0x01`.
fn edges(front: &[Value], bound: Option<(Reach, &Value)>) -> Option<(Vec<u8>, Vec<u8>)> {
    const VALUE: u8 = 0x01;
    const PAST: u8 = 0xFF;
    let mut start = Vec::new();
    if !normalize(front, &mut start) {
        return None;
    }
    let after = |at: &[u8], byte: u8| {
        let mut out = at.to_vec();
        out.push(byte);
        out
    };
    let end = after(&start, PAST);
    let Some((reach, value)) = bound else { return Some((start, end)) };
    let mut equal = start.clone();
    if !normalize(std::slice::from_ref(value), &mut equal) {
        return None;
    }
    let past = after(&equal, PAST);
    let valued = after(&start, VALUE);
    Some(match reach {
        Reach::AtLeast => (equal, end),
        Reach::Above => (past, end),
        Reach::AtMost => (valued, past),
        Reach::Below => (valued, equal),
    })
}

/// How a read of a range over the front of a key orders its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// In the order of the rows, for a statement with no `ORDER BY`.
    Rows,
    /// In the order of the key's columns after the front, the highest first when `true`.
    Key(bool),
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
                        && built.fits(rows, placed, key, true) =>
                {
                    built.as_ref()
                }
                _ => {
                    again = self.again((which, true), rows, placed, key, fresh)?;
                    again.as_ref()
                }
            };
            let Some((part, place, number)) = located.find(wanted, &scratch, values) else {
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
                    keys.push(if located.unique {
                        match encode(chunk, &key, row, &mut scratch)? {
                            Encoded::Null => Noted::Null,
                            Encoded::Int(key) => Noted::Int(key),
                            Encoded::Bytes => Noted::Bytes(scratch.as_slice().into()),
                        }
                    } else {
                        Noted::Null
                    });
                }
            }
            let normal = match located.normal.get() {
                Some(Some(_)) => normalized(chunks, &located.columns)?,
                _ => None,
            };
            appending.push((which, keys, normal));
        }
        Ok(Appending(appending))
    }

    /// Notes the keys of the rows just appended, which [`Self::appending`] read before the append,
    /// into what is built, for a table that keeps its placing across the append. `before` is how
    /// many rows there were and `from` the first part the append can have changed.
    ///
    /// What is built for a key that copies of the table share, because a transaction holds one, is
    /// copied first, which shares its keys and adds a level of its own (see [`Located`]). Anything
    /// that does not come out right is dropped.
    pub(crate) fn appended(&mut self, appending: Appending, rows: &Rows, before: u64, from: usize) {
        self.again.get_mut().unwrap_or_else(PoisonError::into_inner).clear();
        let mut appending = appending.0.into_iter().peekable();
        for (which, slot) in self.built.iter_mut().enumerate() {
            let keys =
                appending.next_if(|(at, ..)| *at == which).map(|(_, keys, normal)| (keys, normal));
            let kept = match (slot.get_mut(), keys) {
                (Some(located), Some((keys, normal))) => {
                    unshared(located).extend(rows, before, from, keys, normal).unwrap_or(false)
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

    /// Notes that the rows `numbers` names, which rise, are taken out of `rows`, for a table that
    /// keeps its placing across the take, see [`Located::take`]. `moved` says the parts of `rows`
    /// did not stay as they were. What a copy of the table shares is copied first, as for
    /// [`Self::appended`], and anything that does not come out right is dropped.
    pub(crate) fn taken(&mut self, rows: &Rows, numbers: &[u64], moved: bool) {
        self.again.get_mut().unwrap_or_else(PoisonError::into_inner).clear();
        let stale = *self.stale.get_mut();
        for slot in &mut self.built {
            let kept = !stale
                && slot.get_mut().is_some_and(|located| {
                    unshared(located).take(rows, numbers, moved).unwrap_or(false)
                });
            if !kept {
                *slot = OnceLock::new();
            }
        }
        if self.built.iter().all(|slot| slot.get().is_none()) {
            *self.stale.get_mut() = false;
        }
    }

    /// How many rows were taken out of what is built for the key at `which`, if it is built.
    #[cfg(test)]
    pub(crate) fn taken_out(&self, which: usize) -> Option<usize> {
        Some(self.built.get(which)?.get()?.taken.len())
    }

    /// Puts the levels of what is built back into one where nothing else holds the first, which is
    /// after the transaction whose copy shared them is done.
    pub(crate) fn settle(&mut self) {
        for slot in &mut self.built {
            if let Some(located) = slot.get_mut().and_then(Arc::get_mut) {
                located.settle();
            }
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
                        && built.fits(rows, placed, &held, true) =>
                {
                    built.as_ref()
                }
                _ => {
                    again = self.again((which, true), rows, placed, &held, fresh)?;
                    again.as_ref()
                }
            };
            // Runs of keys of the other kind have none the bound reaches, which happens only for
            // a table with no rows.
            let runs = &located.sorted().0;
            let picked: Vec<(Wanted<'_>, u64)> = match bound {
                Edge::Int(bound) => {
                    let windows: Vec<_> = runs
                        .iter()
                        .filter_map(|run| match &**run {
                            Sorted::Ints(run) => Some(window(run, reach, |(at, _)| at.cmp(&bound))),
                            Sorted::Text(_) => None,
                        })
                        .collect();
                    pick(
                        &windows,
                        descending,
                        limit,
                        |&(at, _)| at,
                        |&(at, number)| Some((Wanted::Int(at), located.number(number)?)),
                    )
                }
                Edge::Text(bound) => {
                    let windows: Vec<_> = runs
                        .iter()
                        .filter_map(|run| match &**run {
                            Sorted::Text(run) => {
                                Some(window(run, reach, |(at, _)| (**at).cmp(bound.as_bytes())))
                            }
                            Sorted::Ints(_) => None,
                        })
                        .collect();
                    pick(
                        &windows,
                        descending,
                        limit,
                        |(at, _)| &**at,
                        |(at, number)| Some((Wanted::Text(at), located.number(*number)?)),
                    )
                }
            };
            let mut chunks = Vec::with_capacity(picked.len());
            for (wanted, number) in picked {
                // The number of the row now, which is where it is.
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

    /// The rows of `rows` whose first columns of the key over `key` hold `front`, and whose next
    /// column is on the `reach` side of the value of `bound` when there is one, at most `limit` of
    /// them in `order`, with the columns `columns`, one row to a chunk.
    ///
    /// `None` when that would not answer what the plan does: a value or a column of the key is of
    /// a type the normalized form does not write, an order by the next column would meet a null
    /// there, which the plan puts last and the normalized form first, or `order` is
    /// [`Order::Rows`] and more than `most` keys hold the front.
    ///
    /// The caller has made sure `key` is the key or the index at `which`, which `unique` says is a
    /// key or a unique index, and that every value passes [`looks_up`] against its column.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prefix(
        &self,
        (which, unique): (usize, bool),
        rows: &Rows,
        placed: u64,
        key: &[usize],
        front: &[Value],
        bound: Option<(Reach, &Value)>,
        order: Order,
        (limit, most): (usize, usize),
        columns: &[usize],
    ) -> Result<Option<Vec<Chunk>>> {
        let Some((low, high)) = edges(front, bound) else { return Ok(None) };
        // A null in the next column holds the front, and a bound leaves it out. With no bound the
        // window starts at the front itself, and the next column after it.
        let nulls = bound.is_none() && key.len() > front.len() && order != Order::Rows;
        let depth = low.len();
        let mut read = columns.to_vec();
        read.extend_from_slice(key);
        let mut values = Vec::with_capacity(key.len());
        let mut held = Vec::new();
        let mut fresh = false;
        'again: loop {
            let again;
            let built = self.built.get(which).and_then(OnceLock::get);
            let located = match built {
                Some(built)
                    if !fresh
                        && !self.stale.load(Ordering::Relaxed)
                        && built.fits(rows, placed, key, unique) =>
                {
                    built.as_ref()
                }
                _ => {
                    again = self.again((which, unique), rows, placed, key, fresh)?;
                    again.as_ref()
                }
            };
            let Some(normal) = located.normal(rows)? else { return Ok(None) };
            let windows: Vec<&[NormalKey]> = normal
                .0
                .iter()
                .map(|run| {
                    let start = run.partition_point(|(at, _)| **at < *low);
                    let end = run.partition_point(|(at, _)| **at < *high).max(start);
                    &run[start..end]
                })
                .collect();
            // A null sorts first, so it would be at the front of a window.
            if nulls
                && windows
                    .iter()
                    .any(|window| window.first().is_some_and(|(at, _)| at.get(depth) == Some(&0)))
            {
                return Ok(None);
            }
            let picked: Vec<(&[u8], u64)> = match order {
                Order::Key(descending) => pick(
                    &windows,
                    descending,
                    limit,
                    |(at, number)| (&**at, *number),
                    |(at, number)| Some((&**at, located.number(*number)?)),
                ),
                Order::Rows => {
                    if windows.iter().map(|window| window.len()).sum::<usize>() > most {
                        return Ok(None);
                    }
                    let mut all: Vec<(&[u8], u64)> = windows
                        .iter()
                        .flat_map(|window| window.iter())
                        .filter_map(|(at, number)| Some((&**at, located.number(*number)?)))
                        .collect();
                    all.sort_unstable_by_key(|&(_, number)| number);
                    all.truncate(limit);
                    all
                }
            };
            let mut chunks = Vec::with_capacity(picked.len());
            for (wanted, number) in picked {
                let found = match located.place(number) {
                    Some((part, place)) if (place as usize) < rows.chunk_len(part)? => {
                        let chunk = rows.read_selected(part, &read, &[place])?;
                        values.clear();
                        for at in columns.len()..read.len() {
                            values.push(chunk.column(at)?.value_at(0));
                        }
                        held.clear();
                        (normalize(&values, &mut held) && held == wanted).then_some(chunk)
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
            return Ok(Some(chunks));
        }
    }

    /// The rows' keys over `key` at `placed` when `built` cannot answer: built into `built` the
    /// first time, and otherwise into `again` when what that holds is for anything else or when
    /// `fresh` says what was used was wrong. `unique` says whether the key at `which` is a key or a
    /// unique index.
    fn again(
        &self,
        (which, unique): (usize, bool),
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
            let located = Arc::new(opened_or_built(rows, key, placed, unique)?);
            return Ok(Arc::clone(slot.get_or_init(|| located)));
        } else if let Some(located) = held.iter().find(|held| held.fits(rows, placed, key, unique))
        {
            return Ok(Arc::clone(located));
        }
        // Once what was used turned out wrong, the keys are read from the rows and not from a run.
        let located = Arc::new(if fresh {
            Located::build(rows, key, placed, unique)?
        } else {
            opened_or_built(rows, key, placed, unique)?
        });
        held.retain(|held| held.columns != key);
        held.push(Arc::clone(&located));
        Ok(located)
    }
}
