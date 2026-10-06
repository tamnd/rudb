//! A run: a sorted, immutable sequence of index entries in blocks, with the first key of every
//! block (its fence) and a bloom filter held apart from the blocks (section 11.6).
//!
//! A block is up to [`BLOCK`] bytes of entries. Each entry is the length of the prefix it shares
//! with the key before it in the block, the length of the rest and the rest, then the rid and the
//! ts, every number a LEB128 varint. The first entry of a block shares nothing, so a block reads
//! on its own. A point lookup tests the bloom filter, finds the block by a binary search of the
//! fences and reads that one block.
//!
//! Entries with equal keys are allowed and keep the order they were written in. An entry is a hint
//! that a row had the key (section 11.3), so a key can have one entry for the rid a row has now and
//! one for a rid it had before, and it is the caller that checks which one is still true.
//!
//! [`Run::encode`] writes a run as a `RUDBKI1` file and [`Run::decode`] reads one back. The fences
//! and the bloom filter come first and are covered by a checksum, so opening a run reads them and
//! not the blocks.

use std::cmp::Ordering;
use std::ops::Bound;

use rudb_common::{Error, Result};

use crate::bloom::{self, Bloom};

/// The bytes a block holds at most, unless one entry alone is longer.
pub const BLOCK: usize = 4096;

/// What a `RUDBKI1` file starts with.
const MAGIC: &[u8; 8] = b"RUDBKI1\0";

/// Where an entry says a row with its key was, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The row.
    pub rid: u64,
    /// The commit timestamp of the write that made the entry.
    pub ts: u64,
}

/// A sorted, immutable run of index entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The blocks, one after another.
    blocks: Box<[u8]>,
    /// Where each block starts in `blocks`, and the end of the last one.
    starts: Box<[usize]>,
    /// The first key of each block, one after another.
    fences: Box<[u8]>,
    /// Where each fence starts in `fences`, and the end of the last one.
    fence_starts: Box<[usize]>,
    bloom: Bloom,
    len: u64,
}

/// Builds a [`Run`] from entries given in key order.
#[derive(Debug, Default)]
pub struct RunWriter {
    blocks: Vec<u8>,
    starts: Vec<usize>,
    fences: Vec<u8>,
    fence_starts: Vec<usize>,
    hashes: Vec<u64>,
    last: Vec<u8>,
    len: u64,
}

impl RunWriter {
    /// A writer with nothing in it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an entry. Its key has to be at least the key of the entry before it.
    ///
    /// # Errors
    ///
    /// When `key` sorts before the key written last.
    pub fn push(&mut self, key: &[u8], hit: Hit) -> Result<()> {
        let (mut shared, fresh) = match (self.len, key.cmp(&self.last)) {
            (0, _) => (0, true),
            (_, Ordering::Less) => {
                return Err(Error::internal("index run entries out of key order"));
            }
            (_, Ordering::Equal) => (key.len(), false),
            (_, Ordering::Greater) => (common(key, &self.last), true),
        };
        if fresh {
            self.hashes.push(bloom::hash(key));
        }
        let start = self.starts.last().copied().unwrap_or(0);
        let size = varint_len(shared as u64)
            + varint_len((key.len() - shared) as u64)
            + (key.len() - shared)
            + varint_len(hit.rid)
            + varint_len(hit.ts);
        if self.starts.is_empty() || self.blocks.len() - start + size > BLOCK {
            self.starts.push(self.blocks.len());
            self.fence_starts.push(self.fences.len());
            self.fences.extend_from_slice(key);
            shared = 0;
        }
        put_varint(&mut self.blocks, shared as u64);
        put_varint(&mut self.blocks, (key.len() - shared) as u64);
        self.blocks.extend_from_slice(&key[shared..]);
        put_varint(&mut self.blocks, hit.rid);
        put_varint(&mut self.blocks, hit.ts);
        self.last.clear();
        self.last.extend_from_slice(key);
        self.len += 1;
        Ok(())
    }

    /// The run of the entries pushed.
    #[must_use]
    pub fn finish(mut self) -> Run {
        let mut bloom = Bloom::sized(self.hashes.len());
        for &hash in &self.hashes {
            bloom.insert(hash);
        }
        self.starts.push(self.blocks.len());
        self.fence_starts.push(self.fences.len());
        Run {
            blocks: self.blocks.into(),
            starts: self.starts.into(),
            fences: self.fences.into(),
            fence_starts: self.fence_starts.into(),
            bloom,
            len: self.len,
        }
    }
}

impl Run {
    /// How many entries the run holds.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the run holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many blocks the run has.
    #[must_use]
    pub fn blocks(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The bytes the run keeps in memory whatever is read: its fences and its bloom filter.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.fences.len()
            + self.fence_starts.len() * 8
            + self.starts.len() * 8
            + self.bloom.words().len() * 8
    }

    fn fence(&self, block: usize) -> &[u8] {
        &self.fences[self.fence_starts[block]..self.fence_starts[block + 1]]
    }

    /// Whether the bloom filter lets `key` through. `false` means no entry has the key.
    #[must_use]
    pub fn may_hold(&self, key: &[u8]) -> bool {
        self.bloom.may_hold(bloom::hash(key))
    }

    /// The entries with `key`, in the order they were written, each handed to `accept` until it
    /// answers `true`, and that one. The bloom filter is asked first.
    pub fn find(&self, key: &[u8], mut accept: impl FnMut(Hit) -> bool) -> Option<Hit> {
        if self.is_empty() || !self.may_hold(key) {
            return None;
        }
        let mut cursor = self.seek(key);
        while cursor.advance() {
            if cursor.key() != key {
                return None;
            }
            if accept(cursor.hit()) {
                return Some(cursor.hit());
            }
        }
        None
    }

    /// A cursor before the first entry.
    #[must_use]
    pub fn cursor(&self) -> Cursor<'_> {
        Cursor::new(self, 0)
    }

    /// A cursor before the first entry whose key is at least `key`.
    ///
    /// Entries with one key can run over the end of a block, so the search starts in the block
    /// before the first whose fence is not less than `key`, and the cursor reads past the smaller
    /// keys there.
    #[must_use]
    pub fn seek(&self, key: &[u8]) -> Cursor<'_> {
        let blocks = self.blocks();
        let after = partition(blocks, |block| self.fence(block) < key);
        let mut cursor = Cursor::new(self, after.saturating_sub(1));
        cursor.skip_below(key);
        cursor
    }

    /// The entries whose keys are within `lo` and `hi`, in key order, each with its key.
    pub fn range<'a>(&'a self, lo: Bound<&[u8]>, hi: Bound<&'a [u8]>) -> Range<'a> {
        let cursor = match lo {
            Bound::Unbounded => self.cursor(),
            Bound::Included(key) => self.seek(key),
            Bound::Excluded(key) => {
                let mut cursor = self.seek(key);
                cursor.skip_equal(key);
                cursor
            }
        };
        Range { cursor, hi, done: false }
    }

    /// The run written as a `RUDBKI1` file.
    ///
    /// The header is the magic, then as `u64` the entries, the blocks, the bloom words, the fence
    /// bytes and the block bytes. The block starts and the fence starts follow, a `u64` each and
    /// one more than there are blocks, then the bloom words, then the fences, then a check of
    /// everything so far, then the blocks. Every number is little-endian.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let blocks = self.blocks();
        let words = self.bloom.words();
        let mut out = Vec::with_capacity(self.resident_bytes() + self.blocks.len() + 64);
        out.extend_from_slice(MAGIC);
        for n in [
            self.len,
            blocks as u64,
            words.len() as u64,
            self.fences.len() as u64,
            self.blocks.len() as u64,
        ] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        for &start in self.starts.iter().chain(self.fence_starts.iter()) {
            out.extend_from_slice(&(start as u64).to_le_bytes());
        }
        for &word in words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&self.fences);
        let check = bloom::hash(&out);
        out.extend_from_slice(&check.to_le_bytes());
        out.extend_from_slice(&self.blocks);
        out
    }

    /// A run read back from what [`Run::encode`] wrote.
    ///
    /// The header, the fences and the bloom filter are checked, and the blocks are not read: a
    /// block that was damaged on disk reads as one that ends early, and never as a panic.
    ///
    /// # Errors
    ///
    /// When `bytes` is not a whole `RUDBKI1` run or its check does not match.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader { bytes, at: 0 };
        if reader.take(8)? != MAGIC {
            return Err(corrupt("not a key index run"));
        }
        let len = reader.u64()?;
        let blocks = reader.size()?;
        let words = reader.size()?;
        let fence_bytes = reader.size()?;
        let block_bytes = reader.size()?;
        let starts_len = blocks + 1;
        if (blocks == 0) != (len == 0) || blocks as u64 > len {
            return Err(corrupt("blocks do not match the entries"));
        }
        let starts = reader.starts(starts_len, block_bytes)?;
        let fence_starts = reader.starts(starts_len, fence_bytes)?;
        let mut bloom = Vec::with_capacity(words.min(bytes.len() / 8));
        for _ in 0..words {
            bloom.push(reader.u64()?);
        }
        let fences: Box<[u8]> = reader.take(fence_bytes)?.into();
        let checked = reader.at;
        if reader.u64()? != bloom::hash(&bytes[..checked]) {
            return Err(corrupt("the check does not match"));
        }
        let blocks: Box<[u8]> = reader.take(block_bytes)?.into();
        if reader.at != bytes.len() {
            return Err(corrupt("bytes after the blocks"));
        }
        let bloom = Bloom::from_words(bloom.into()).ok_or_else(|| corrupt("no bloom filter"))?;
        let run = Self { blocks, starts, fences, fence_starts, bloom, len };
        let blocks = run.blocks();
        if (1..blocks).any(|block| run.fence(block - 1) > run.fence(block)) {
            return Err(corrupt("fences out of order"));
        }
        Ok(run)
    }

    /// Merges `runs`, given newest first, into one run, keeping the entries `keep` answers `true`
    /// for. Entries with equal keys come out newest run first, and in their order within a run.
    ///
    /// This is compaction (section 11.6): `keep` is where entries of aborted transactions, of rows
    /// dead to every snapshot and of rows that are gone are dropped.
    ///
    /// # Panics
    ///
    /// Never for runs built by a [`RunWriter`] or read by [`Run::decode`], which are sorted.
    #[must_use]
    pub fn merge(runs: &[&Run], mut keep: impl FnMut(&[u8], Hit) -> bool) -> Run {
        let mut cursors: Vec<_> = runs.iter().map(|run| run.cursor()).collect();
        let mut live: Vec<bool> = cursors.iter_mut().map(Cursor::advance).collect();
        let mut writer = RunWriter::new();
        loop {
            let mut least: Option<usize> = None;
            for (at, cursor) in cursors.iter().enumerate() {
                if live[at] && least.is_none_or(|least| cursor.key() < cursors[least].key()) {
                    least = Some(at);
                }
            }
            let Some(at) = least else { break };
            let cursor = &mut cursors[at];
            if keep(cursor.key(), cursor.hit()) {
                writer.push(cursor.key(), cursor.hit()).expect("a merge of sorted runs is sorted");
            }
            live[at] = cursor.advance();
        }
        writer.finish()
    }
}

/// Reads the entries of a run in order.
#[derive(Debug)]
pub struct Cursor<'a> {
    run: &'a Run,
    /// The block being read.
    block: usize,
    /// Where the next entry starts in the run's blocks.
    at: usize,
    key: Vec<u8>,
    hit: Hit,
    /// Whether the entry the cursor is on is the one [`Cursor::advance`] answers next, because a
    /// skip stopped on it.
    held: bool,
}

impl<'a> Cursor<'a> {
    fn new(run: &'a Run, block: usize) -> Self {
        let at = run.starts.get(block).copied().unwrap_or(run.blocks.len());
        Self { run, block, at, key: Vec::new(), hit: Hit { rid: 0, ts: 0 }, held: false }
    }

    /// Moves to the next entry, and answers whether there was one.
    pub fn advance(&mut self) -> bool {
        if self.held {
            self.held = false;
            return true;
        }
        let run = self.run;
        let blocks = run.blocks();
        while self.block < blocks && self.at >= run.starts[self.block + 1] {
            self.block += 1;
        }
        if self.block >= blocks {
            return false;
        }
        let end = run.starts[self.block + 1];
        let first = self.at == run.starts[self.block];
        let bytes = &run.blocks[..end];
        let mut at = self.at;
        let entry = (|| {
            let shared = usize::try_from(varint(bytes, &mut at)?).ok()?;
            let rest = usize::try_from(varint(bytes, &mut at)?).ok()?;
            if shared > self.key.len() || (first && shared != 0) {
                return None;
            }
            let suffix = bytes.get(at..at.checked_add(rest)?)?;
            at += rest;
            self.key.truncate(shared);
            self.key.extend_from_slice(suffix);
            let rid = varint(bytes, &mut at)?;
            let ts = varint(bytes, &mut at)?;
            Some(Hit { rid, ts })
        })();
        match entry {
            Some(hit) => {
                self.hit = hit;
                self.at = at;
                true
            }
            None => {
                // A damaged block: stop here rather than read nonsense.
                self.block = blocks;
                false
            }
        }
    }

    /// The key of the entry the cursor is on.
    #[must_use]
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The rid and ts of the entry the cursor is on.
    #[must_use]
    pub fn hit(&self) -> Hit {
        self.hit
    }

    /// Reads past the entries whose keys are less than `key`, leaving the cursor before the first
    /// that is not.
    fn skip_below(&mut self, key: &[u8]) {
        self.skip_while(|entry| entry < key);
    }

    /// Reads past the entries whose keys are `key`.
    fn skip_equal(&mut self, key: &[u8]) {
        self.skip_while(|entry| entry == key);
    }

    fn skip_while(&mut self, mut skip: impl FnMut(&[u8]) -> bool) {
        while self.advance() {
            if !skip(&self.key) {
                self.held = true;
                return;
            }
        }
    }
}

/// The entries of a run between two bounds, from [`Run::range`].
#[derive(Debug)]
pub struct Range<'a> {
    cursor: Cursor<'a>,
    hi: Bound<&'a [u8]>,
    done: bool,
}

impl Range<'_> {
    /// The next entry, its key and where it points, or `None` past the upper bound.
    pub fn next_entry(&mut self) -> Option<(&[u8], Hit)> {
        if self.done || !self.cursor.advance() {
            self.done = true;
            return None;
        }
        let key = self.cursor.key();
        let within = match self.hi {
            Bound::Unbounded => true,
            Bound::Included(hi) => key <= hi,
            Bound::Excluded(hi) => key < hi,
        };
        if !within {
            self.done = true;
            return None;
        }
        Some((self.cursor.key(), self.cursor.hit()))
    }
}

/// The first of `0..n` for which `before` is false, given that it is true up to some point and
/// false after it.
fn partition(n: usize, mut before: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

/// The length of the prefix `a` and `b` share.
fn common(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(a, b)| a == b).count()
}

fn varint_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 0x80 {
        n >>= 7;
        len += 1;
    }
    len
}

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        n |= u64::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(n);
        }
    }
    None
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| corrupt("cut short"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    /// A count of things that are in the file, so at most its length.
    fn size(&mut self) -> Result<usize> {
        usize::try_from(self.u64()?)
            .ok()
            .filter(|&n| n <= self.bytes.len())
            .ok_or_else(|| corrupt("a size past the end"))
    }

    /// `n` offsets into a section of `len` bytes, starting at 0, never going down and ending at
    /// `len`.
    fn starts(&mut self, n: usize, len: usize) -> Result<Box<[usize]>> {
        let mut starts = Vec::with_capacity(n);
        for _ in 0..n {
            let start = self.size()?;
            if start > len || starts.last().is_some_and(|&last| start < last) {
                return Err(corrupt("offsets out of order"));
            }
            starts.push(start);
        }
        if starts.first() != Some(&0) || starts.last() != Some(&len) {
            return Err(corrupt("offsets do not cover the section"));
        }
        Ok(starts.into())
    }
}

fn corrupt(message: &str) -> Error {
    Error::invalid_input(format!("invalid rudb key index run: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u64) -> Vec<u8> {
        format!("user{i:08}").into_bytes()
    }

    fn hit(rid: u64) -> Hit {
        Hit { rid, ts: rid * 3 }
    }

    fn written(n: u64, step: u64) -> Run {
        let mut writer = RunWriter::new();
        for i in (0..n).map(|i| i * step) {
            writer.push(&key(i), hit(i)).expect("in order");
        }
        writer.finish()
    }

    #[test]
    fn finds_every_key_it_holds_and_none_it_does_not() {
        let run = written(50_000, 2);
        assert_eq!(run.len(), 50_000);
        assert!(run.blocks() > 100, "{} blocks", run.blocks());
        for i in (0..100_000).step_by(7) {
            let found = run.find(&key(i), |_| true);
            assert_eq!(found, (i % 2 == 0).then(|| hit(i)), "{i}");
        }
        assert_eq!(run.find(b"user", |_| true), None);
        assert_eq!(run.find(b"zzz", |_| true), None);
        assert_eq!(run.find(b"", |_| true), None);
    }

    #[test]
    fn hands_over_every_entry_of_a_key_until_one_is_taken() {
        let mut writer = RunWriter::new();
        writer.push(b"a", hit(1)).expect("in order");
        // Enough entries of one key to run over several blocks.
        for rid in 0..2000 {
            writer.push(b"b", hit(100 + rid)).expect("in order");
        }
        writer.push(b"c", hit(2)).expect("in order");
        assert!(writer.push(b"bb", hit(3)).is_err());
        let run = writer.finish();
        assert!(run.blocks() > 2);
        let mut seen = Vec::new();
        assert_eq!(
            run.find(b"b", |hit| {
                seen.push(hit.rid);
                false
            }),
            None
        );
        assert_eq!(seen, (100..2100).collect::<Vec<_>>());
        assert_eq!(run.find(b"b", |hit| hit.rid == 1500), Some(hit(1500)));
        assert_eq!(run.find(b"c", |_| true), Some(hit(2)));
    }

    #[test]
    fn reads_a_range_in_key_order() {
        let run = written(10_000, 3);
        let collect = |lo: Bound<&[u8]>, hi: Bound<&[u8]>| {
            let mut range = run.range(lo, hi);
            let mut rids = Vec::new();
            while let Some((found, hit)) = range.next_entry() {
                assert_eq!(found, key(hit.rid));
                rids.push(hit.rid);
            }
            rids
        };
        let (lo, hi) = (&key(300)[..], &key(330)[..]);
        assert_eq!(
            collect(Bound::Included(lo), Bound::Excluded(hi)),
            (300..330).step_by(3).collect::<Vec<_>>()
        );
        assert_eq!(
            collect(Bound::Excluded(lo), Bound::Included(hi)),
            (303..=330).step_by(3).collect::<Vec<_>>()
        );
        let between = key(301);
        assert_eq!(
            collect(Bound::Included(&between[..]), Bound::Included(hi)),
            (303..=330).step_by(3).collect::<Vec<_>>()
        );
        assert_eq!(collect(Bound::Unbounded, Bound::Excluded(&key(9)[..])), [0, 3, 6]);
        assert_eq!(collect(Bound::Included(&key(29_995)[..]), Bound::Unbounded), [29_997]);
        assert!(collect(Bound::Included(hi), Bound::Excluded(lo)).is_empty());
    }

    #[test]
    fn reads_back_what_it_wrote_and_refuses_damage() {
        let run = written(20_000, 1);
        let bytes = run.encode();
        let back = Run::decode(&bytes).expect("a whole run");
        assert_eq!(back, run);
        assert_eq!(back.find(&key(12_345), |_| true), Some(hit(12_345)));

        let empty = RunWriter::new().finish();
        assert_eq!(Run::decode(&empty.encode()).expect("an empty run"), empty);
        assert_eq!(empty.find(b"a", |_| true), None);

        let header = 8 + 5 * 8 + 3;
        for at in [0, 9, header, header + 8 * run.starts.len(), bytes.len() - run.blocks.len() - 1]
        {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x40;
            assert!(Run::decode(&damaged).is_err(), "a byte changed at {at}");
        }
        for cut in [0, 7, 100, bytes.len() - 1] {
            assert!(Run::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        // The blocks are not checked at open, and a damaged one only ends early.
        let mut damaged = bytes;
        let first_block = damaged.len() - run.blocks.len();
        damaged[first_block..first_block + 64].fill(0xFF);
        let damaged = Run::decode(&damaged).expect("the blocks are not read");
        assert_eq!(damaged.find(&key(0), |_| true), None);
        assert_eq!(damaged.find(&key(19_000), |_| true), Some(hit(19_000)));
    }

    #[test]
    fn merges_runs_newest_first_and_drops_what_it_is_told_to() {
        let mut older = RunWriter::new();
        let mut newer = RunWriter::new();
        for i in 0..1000 {
            older.push(&key(i), Hit { rid: i, ts: 1 }).expect("in order");
            if i % 10 == 0 {
                newer.push(&key(i), Hit { rid: i + 5000, ts: 2 }).expect("in order");
            }
        }
        newer.push(&key(5000), Hit { rid: 9, ts: 2 }).expect("in order");
        let (older, newer) = (older.finish(), newer.finish());
        let merged = Run::merge(&[&newer, &older], |_, hit| hit.rid % 7 != 3);
        let mut cursor = merged.cursor();
        let mut entries = Vec::new();
        while cursor.advance() {
            entries.push((cursor.key().to_vec(), cursor.hit()));
        }
        let mut expected = Vec::new();
        for i in 0..1000 {
            if i % 10 == 0 {
                expected.push((key(i), Hit { rid: i + 5000, ts: 2 }));
            }
            expected.push((key(i), Hit { rid: i, ts: 1 }));
        }
        expected.push((key(5000), Hit { rid: 9, ts: 2 }));
        expected.retain(|(_, hit)| hit.rid % 7 != 3);
        assert_eq!(entries, expected);
        assert_eq!(merged.len(), expected.len() as u64);
        assert_eq!(merged.find(&key(20), |_| true), Some(Hit { rid: 5020, ts: 2 }));
    }
}
