//! The hash table a join finds its gathered rows in.
//!
//! A join and a group by ask a hash table two different questions. A group by asks which slot a key
//! has, one slot per distinct key, and [`crate::table::Table`] is exactly that. A join asks which
//! rows a key has, which is a list, and the two are one structure apart: a table from key to slot,
//! and a chain per slot that threads the rows of that key together. That is what this is. The table
//! does the hashing, the probing and the key comparison, all of it column at a time and a batch at
//! a time, and the chain beside it turns the slot it answers with into the rows the join wanted.
//!
//! The chain is two arrays and no allocation per key. `head[slot]` is the first row of a key and
//! `next[row]` is the row after that one, so a key with a thousand matches costs a thousand `u32`
//! in a run that was allocated once, rather than a `Vec` per distinct key that the allocator has to
//! be asked for and that a probe has to chase a pointer to reach. What this replaces was a
//! `HashMap<Vec<Value>, Vec<usize>>`, which asked the allocator twice per distinct key and once per
//! driving row, hashed a row of tagged values one value at a time, and compared keys the same way.
//!
//! The rows come out of a chain in the order the gathered side holds them, and that is deliberate
//! rather than incidental. The nested loop this replaces produced a driving row's matches in that
//! order, so keeping it makes this a faster way to the same answer instead of the same answer in a
//! different order, and makes a failing test a diff rather than an investigation. A chain that is
//! pushed onto at the front comes out backwards, so the build keeps a tail per slot and appends.
//!
//! # Partitions
//!
//! One table cannot be filled from several threads, because two rows of one key have to reach
//! their chain in the order they arrived. A side split by the top bits of its hash is several
//! tables that share nothing, because a key lands in exactly one of them, and that is how this is
//! built on more than one thread. See [`Lookup::build`]. The split is on the top bits and the
//! bucket inside a partition is the bottom ones, so the two never say the same thing, and the
//! order a chain comes out in does not change: a partition reads the side from the first row to
//! the last, so its chains are in the order the side holds them whatever the other partitions are
//! doing beside it.
//!
//! # Keys that index
//!
//! A join on one integer key whose gathered keys sit close together, which is a join against a
//! primary key in nearly every schema, needs no table at all. The key less the smallest key is a
//! place in `head`, so the probe is a subtraction, a bounds check and a load, with no hash to take
//! and no stored key to compare, and the build is one pass over the side. The slot a probe hands
//! out is that place, and everything past the slot, the chains, the single key test and the
//! gathers, is the same code for both. See [`Lookup::build`].
//!
//! # Nulls
//!
//! `NULL = NULL` is null and not true, so a row whose key holds a null in a column the join
//! compares with `=` matches nothing, on either side. Such a row is not put in the table and not
//! looked up in it, which is what says so. `IS NOT DISTINCT FROM` is the other rule for the same
//! value and its nulls are stored and compared, which the table already does, because a group by
//! puts every null in one group and that is the same question.

use std::sync::atomic::{AtomicU32, Ordering};

use rudb_common::{Cancel, Error, LogicalType, Result};
use rudb_pipeline::Lease;
use rudb_vector::{Form, Vector};

use crate::pairs::in_parallel;
use crate::table::{BATCH, Probe, Table, Walk};

/// The end of a chain, and the row a slot with nothing in it points at.
pub(crate) const NONE: u32 = u32::MAX;

/// How many places in `head` a key may take in the direct form, at most, for each gathered row.
///
/// `head` is four bytes a place and the build keeps a tail beside it, so at four places a row the
/// two cost thirty two bytes a gathered row, which is about what the hash table costs for one row
/// of a single integer key with its hash and bucket. TPC-H's order keys are one in four of the
/// values they span, and every other key it joins on is dense.
const PLACES: u64 = 4;

/// Below this many rows a side is built on one thread.
///
/// Partitioning costs a pass over the hashes per partition and a table per partition, and on a side
/// of a few thousand rows that is more than the build. The number is where the two meet on the
/// shapes in TPC-H rather than anything deeper: q21's two large joins are millions of rows and
/// every join against `nation` or `region` is tens.
const SPLIT: usize = 64 * 1024;

/// What a probe of a row whose key is not in the table leaves behind.
///
/// Distinct from a slot rather than encoded as one, because slot zero is a real key and this has to
/// survive being written into the same run.
pub(crate) const MISS: usize = usize::MAX;

/// One partition of the table, built by one thread and probed by whoever lands in it.
#[derive(Debug)]
struct Part {
    /// One slot per distinct key of this partition.
    table: Table,
    /// What this partition's slots are numbered from once the partitions are laid end to end, so
    /// that a slot handed out by a probe names a key of the whole side rather than of a partition.
    base: usize,
}

/// The gathered side's rows, by the values the key expressions produce from them.
#[derive(Debug, Default)]
pub(crate) struct Lookup {
    /// The partitions, by the top bits of the hash. Empty for a side with nothing keyed in it.
    parts: Vec<Part>,
    /// How many of the hash's top bits name the partition. Zero when there is only one.
    bits: u32,
    /// The first gathered row of each distinct key, by slot, partitions laid end to end.
    head: Vec<u32>,
    /// The next gathered row with the same key, by gathered row, [`NONE`] at the end of a chain.
    ///
    /// Atomic because the partitions write it at the same time, and free because they never write
    /// the same entry: a row belongs to the partition its hash names and to no other.
    next: Vec<AtomicU32>,
    /// How many gathered rows are in the table, which is not how many went past it: a row whose key
    /// holds a rejected null is neither stored nor counted.
    kept: usize,
    /// How many distinct keys the table holds.
    distinct: usize,
    /// The smallest key, when the key is its own place in `head` and there are no partitions. See
    /// the module documentation.
    low: Option<i64>,
}

impl Lookup {
    /// The table over a gathered side whose key columns are `keys`, built on the threads in hand.
    ///
    /// The keys are one run per column rather than a list of chunks, because a partition's rows are
    /// scattered through the side and a thread building one has to reach any of them. See
    /// [`laid_out`](crate::side::laid_out), which is what lays them out.
    ///
    /// # What a partition is for
    ///
    /// Every instance of the pipeline this table belongs to probes it, and until #952 whichever
    /// instance reached it first built the whole thing while the others slept. #952 moved the build
    /// out in front of the instances so that the threads are free during it, and this is what
    /// spends them. Two rows of one key have to reach their chain in the order they arrived, so one
    /// table cannot be filled from several threads, but a side split by the top bits of its hash is
    /// several tables that share nothing: a key lands in exactly one of them, so no two threads
    /// ever look at one bucket, one stored key or one chain.
    ///
    /// The top bits rather than the bottom ones because the bottom ones are the bucket number
    /// inside a partition's own table. Partitioning on those would leave every row of a partition
    /// agreeing on the low bits of its bucket, which is one bucket in `parts` used and the rest
    /// empty.
    ///
    /// The rows are dealt into their partitions once, in two passes over the hashes, before any
    /// partition starts. See `deal_rows` for why that is not a pass per partition.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfMemory`] past [`NONE`] gathered rows, which is the row a
    /// chain uses to say it has ended, and whatever storing a key raises. Cancellation is checked a
    /// batch at a time inside each partition.
    pub(crate) fn build(
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        threads: &Lease<'_>,
        cancel: &Cancel,
    ) -> Result<Self> {
        Self::build_among(keys, rows, nulls, None, threads, cancel)
    }

    /// The same, leaving out every row `allowed` says no to, as if its key held a rejected null.
    ///
    /// For a join that knows some of its gathered rows cannot match any driving row. See
    /// [`Probe::narrowed_by`](crate::join::Probe::narrowed_by).
    ///
    /// # Errors
    ///
    /// The ones [`Lookup::build`] raises.
    pub(crate) fn build_among(
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        allowed: Option<&[bool]>,
        threads: &Lease<'_>,
        cancel: &Cancel,
    ) -> Result<Self> {
        if rows >= NONE as usize {
            return Err(too_many_rows());
        }
        if rows == 0 || keys.is_empty() {
            return Ok(Self::default());
        }
        let mut keyed = Vec::new();
        which_are_keyed(keys, rows, nulls, &mut keyed);
        if let Some(allowed) = allowed {
            for (keyed, &allowed) in keyed.iter_mut().zip(allowed) {
                *keyed = *keyed && allowed;
            }
        }
        if let Some(direct) = Self::direct(keys, rows, nulls, &keyed, threads, cancel)? {
            return Ok(direct);
        }
        let mut hashes = Vec::new();
        crate::table::hash(keys, rows, &mut hashes, crate::table::Across::TwoInputs);

        let bits = split_into(rows, threads.degree());
        let count = 1usize << bits;
        let next: Vec<AtomicU32> = (0..rows).map(|_| AtomicU32::new(NONE)).collect();
        let types: Vec<LogicalType> = keys.iter().map(|key| key.logical_type().clone()).collect();
        let (starts, dealt) = deal_rows(&hashes, &keyed, bits, count);
        let one = |part: usize| -> Result<(Table, Vec<u32>, usize)> {
            let mine = &dealt[starts[part]..starts[part + 1]];
            fill(mine, &types, keys, &hashes, &next, cancel)
        };
        let filled = in_parallel(threads, count, threads.degree(), "join table partition", one)?;

        let mut parts = Vec::with_capacity(count);
        let mut head = Vec::new();
        let mut kept = 0;
        for (table, mine, held) in filled {
            parts.push(Part { base: head.len(), table });
            head.extend(mine);
            kept += held;
        }
        let distinct = head.len();
        Ok(Self { parts, bits, head, next, kept, distinct, low: None })
    }

    /// The direct form, when the key is one integer column compared with `=` and its keyed values
    /// span at most [`PLACES`] places a gathered row. `None` for anything else.
    ///
    /// One pass for the range and one to thread the chains, both in row order, so a key's rows come
    /// out of its chain in the order the side holds them, as they do from the table. A side of
    /// [`SPLIT`] rows or more is dealt first into partitions that each own a run of places, the
    /// way the table deals by the top bits of the hash, so each thread fills its own part of
    /// `head` and the only array they share is `next`, where each writes the rows it owns.
    fn direct(
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        keyed: &[bool],
        threads: &Lease<'_>,
        cancel: &Cancel,
    ) -> Result<Option<Self>> {
        let ([key], [false]) = (keys, nulls) else { return Ok(None) };
        if !integer(key.logical_type()) {
            return Ok(None);
        }
        let mut block = Vec::new();
        if !key.signed_block(&mut block) || block.len() < rows {
            return Ok(None);
        }
        let (mut low, mut high) = (i64::MAX, i64::MIN);
        for (&value, &keyed) in block[..rows].iter().zip(keyed) {
            if keyed {
                low = low.min(value);
                high = high.max(value);
            }
        }
        if low > high {
            return Ok(None);
        }
        let Ok(places) = u64::try_from(i128::from(high) - i128::from(low) + 1) else {
            return Ok(None);
        };
        if places > (rows as u64).saturating_mul(PLACES) || places >= u64::from(NONE) {
            return Ok(None);
        }
        cancel.check()?;
        let places = places as usize;
        let place_of = |row: usize| block[row].wrapping_sub(low) as usize;
        let next: Vec<AtomicU32> = (0..rows).map(|_| AtomicU32::new(NONE)).collect();
        let count = 1usize << split_into(rows, threads.degree());
        let run = places.div_ceil(count);
        let mut starts = vec![0; count + 1];
        for row in (0..rows).filter(|&row| keyed[row]) {
            starts[place_of(row) / run + 1] += 1;
        }
        for part in 0..count {
            starts[part + 1] += starts[part];
        }
        let mut at = starts.clone();
        let mut dealt = vec![0; starts[count]];
        for row in (0..rows).filter(|&row| keyed[row]) {
            let part = place_of(row) / run;
            dealt[at[part]] = row;
            at[part] += 1;
        }
        let one = |part: usize| -> Result<(Vec<u32>, usize)> {
            let base = part * run;
            let len = run.min(places.saturating_sub(base));
            let mut head = vec![NONE; len];
            let mut tail = vec![NONE; len];
            let mut distinct = 0;
            for &row in &dealt[starts[part]..starts[part + 1]] {
                let place = place_of(row) - base;
                let at = row as u32;
                if tail[place] == NONE {
                    head[place] = at;
                    distinct += 1;
                } else {
                    next[tail[place] as usize].store(at, Ordering::Relaxed);
                }
                tail[place] = at;
            }
            Ok((head, distinct))
        };
        let filled = in_parallel(threads, count, threads.degree(), "join index partition", one)?;
        let mut head = Vec::with_capacity(places);
        let mut distinct = 0;
        for (mine, held) in filled {
            head.extend(mine);
            distinct += held;
        }
        let kept = dealt.len();
        Ok(Some(Self { parts: Vec::new(), bits: 0, head, next, kept, distinct, low: Some(low) }))
    }

    /// Whether there is anything at all to look up.
    ///
    /// The driving side asks before it evaluates a key expression, so a gathered side with nothing
    /// keyed in it is a join that evaluates nothing on the driving side either. That is not only
    /// the work saved: a key expression that raises on a row is a key expression raising about a
    /// row that could not have matched anything, which the nested loop this replaces never did.
    pub(crate) fn is_empty(&self) -> bool {
        self.kept == 0
    }

    /// What this has taken from the allocator, capacity rather than length throughout.
    pub(crate) fn footprint(&self) -> u64 {
        let tables: u64 =
            self.parts.iter().map(|part| part.table.footprint() + part.table.owned()).sum();
        let chain =
            self.head.capacity() * size_of::<u32>() + self.next.capacity() * size_of::<AtomicU32>();
        tables + u64::try_from(chain).unwrap_or(u64::MAX)
    }

    /// The slot each driving row's key is in, [`MISS`] where it is in none.
    ///
    /// One call per driving chunk rather than one per driving row, which is the whole point: the
    /// hash is a pass per key column and the probe is a batch at a time, so a chunk of two thousand
    /// rows costs two thousand rows of arithmetic and one set of outstanding cache misses per batch
    /// rather than three dependent misses per row.
    ///
    /// With more than one partition the chunk is dealt into them first, because a batch has to be
    /// probed against one table and a driving row's partition is whatever its hash says. The deal
    /// is a pass over the hashes and the batches that come out of it are the same size as before.
    ///
    /// The scratch buffers are the caller's because the caller is one instance of the probe and
    /// this table is shared by all of them.
    pub(crate) fn slots(
        &self,
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        scratch: &mut Scratch,
        into: &mut Vec<usize>,
    ) {
        into.clear();
        into.resize(rows, MISS);
        if let Some(low) = self.low {
            self.places(low, keys, rows, nulls, scratch, into);
            return;
        }
        if rows == 0 || self.parts.is_empty() {
            return;
        }
        crate::table::hash(keys, rows, &mut scratch.hashes, crate::table::Across::TwoInputs);
        which_are_keyed(keys, rows, nulls, &mut scratch.keyed);
        if self.parts.len() == 1 {
            let mut from = 0;
            while from < rows {
                let upto = (from + BATCH).min(rows);
                self.parts[0].table.probe_run(
                    &scratch.hashes,
                    keys,
                    from,
                    upto,
                    into,
                    &mut scratch.walk,
                );
                from = upto;
            }
        } else {
            self.deal(keys, rows, scratch, into);
        }
        // A row whose key holds a rejected null matches nothing, and the table was never told about
        // that rule. It would answer with a miss anyway, because no key holding such a null was
        // ever stored and the comparison against one that does not is false, but a lookup that is
        // right by two steps of reasoning rather than one is a lookup that stops being right when
        // somebody changes the other step.
        for (row, &keyed) in scratch.keyed.iter().enumerate().take(rows) {
            if !keyed {
                into[row] = MISS;
            }
        }
    }

    /// The same, for the direct form, where a key's slot is its place in `head` and a key outside
    /// the range or on a place nobody holds is a miss.
    fn places(
        &self,
        low: i64,
        keys: &[Vector],
        rows: usize,
        nulls: &[bool],
        scratch: &mut Scratch,
        into: &mut [usize],
    ) {
        let [key] = keys else { return };
        which_are_keyed(keys, rows, nulls, &mut scratch.keyed);
        let places = self.head.len() as u64;
        let hit = |value: i64| {
            let place = value.wrapping_sub(low) as u64;
            (place < places && self.head[place as usize] != NONE).then_some(place as usize)
        };
        if key.signed_block(&mut scratch.block) && scratch.block.len() >= rows {
            for (row, &value) in scratch.block[..rows].iter().enumerate() {
                if scratch.keyed[row] {
                    into[row] = hit(value).unwrap_or(MISS);
                }
            }
            return;
        }
        for (row, slot) in into.iter_mut().enumerate().take(rows) {
            if scratch.keyed[row] {
                let value = key.signed_at(row).and_then(|value| i64::try_from(value).ok());
                *slot = value.and_then(hit).unwrap_or(MISS);
            }
        }
    }

    /// The same, for a table in partitions, which has to know which one before it can ask.
    fn deal(&self, keys: &[Vector], rows: usize, scratch: &mut Scratch, into: &mut [usize]) {
        scratch.by_part.resize_with(self.parts.len(), Vec::new);
        for held in &mut scratch.by_part {
            held.clear();
        }
        for row in 0..rows {
            if scratch.keyed[row] {
                scratch.by_part[part_of(scratch.hashes[row], self.bits)].push(row);
            }
        }
        for (part, mine) in self.parts.iter().zip(&scratch.by_part) {
            let mut from = 0;
            while from < mine.len() {
                let upto = (from + BATCH).min(mine.len());
                let batch = &mine[from..upto];
                scratch.found.clear();
                scratch.found.resize(batch.len(), MISS);
                part.table.probe_these(
                    &scratch.hashes,
                    keys,
                    batch,
                    &mut scratch.found,
                    &mut scratch.walk,
                );
                for (&row, &slot) in batch.iter().zip(&scratch.found) {
                    if slot != MISS {
                        into[row] = part.base + slot;
                    }
                }
                from = upto;
            }
        }
    }

    /// How many slots a probe can hand out, which is one past the largest.
    pub(crate) fn slot_count(&self) -> usize {
        self.head.len()
    }

    /// Whether every key in the table holds exactly one gathered row.
    ///
    /// True of a join against a primary key, which is most of the large joins in TPC-H. Every chain
    /// is then its head and nothing after it, so a probe can answer with the head alone and never
    /// read [`Lookup::next`]. That read is a miss into an array as long as the gathered side, taken
    /// once per driving row only to find the end of a chain that has already ended.
    pub(crate) fn single(&self) -> bool {
        self.distinct == self.kept
    }

    /// The first gathered row of each slot in `slots`, [`NONE`] for a [`MISS`].
    ///
    /// A pass over a whole driving chunk before any row is answered, rather than a load per row in
    /// the loop that answers them. Each load is a miss into an array as long as the gathered side
    /// and none of them depends on another, so taken together the processor has many of them in
    /// flight at once, where the row loop had one and waited it out before the next.
    pub(crate) fn firsts(&self, slots: &[usize], into: &mut Vec<u32>) {
        into.clear();
        into.extend(slots.iter().map(|&slot| self.head.get(slot).copied().unwrap_or(NONE)));
    }

    /// The gathered rows of the chain that starts at `first`, which [`Lookup::firsts`] handed out.
    ///
    /// The same as [`Lookup::matches`] from its second step on. `into` is cleared here.
    pub(crate) fn chain_from(&self, first: u32, into: &mut Vec<u32>) {
        into.clear();
        let mut at = first;
        while at != NONE {
            into.push(at);
            at = self.next[at as usize].load(Ordering::Relaxed);
        }
    }

    /// The gathered rows in one slot's chain, in the order the gathered side holds them.
    ///
    /// `into` is the caller's buffer so that a driving row does not cost an allocation, and it is
    /// cleared here rather than by the caller.
    ///
    /// Row numbers rather than indices, because what the caller does with them is hand them to
    /// [`Build::gather`](crate::side::Build::gather), and a gather takes a run of `u32`. The chain
    /// is a run of `u32` already, so this is a copy rather than a widening.
    pub(crate) fn matches(&self, slot: usize, into: &mut Vec<u32>) {
        into.clear();
        if slot == MISS {
            return;
        }
        let mut at = self.head[slot];
        while at != NONE {
            into.push(at);
            at = self.next[at as usize].load(Ordering::Relaxed);
        }
    }
}

/// One partition of the build: the rows whose hash names it, in the order the side holds them.
///
/// Everything here belongs to this partition alone except `next`, and the entries of that it writes
/// are the rows it owns, so nothing it touches is touched by another thread.
#[allow(clippy::too_many_arguments)]
fn fill(
    mine: &[usize],
    types: &[LogicalType],
    keys: &[Vector],
    hashes: &[u64],
    next: &[AtomicU32],
    cancel: &Cancel,
) -> Result<(Table, Vec<u32>, usize)> {
    let mut table = Table::new(types);
    let mut head: Vec<u32> = Vec::new();
    let mut tail: Vec<u32> = Vec::new();
    let mut found: Vec<usize> = Vec::new();
    let mut walk = Walk::default();
    let mut kept = 0;
    let mut from = 0;
    while from < mine.len() {
        // Once per batch rather than once per row. A build over a side nobody bounded is the one
        // part of this operator that can run long without producing anything.
        cancel.check()?;
        let upto = (from + BATCH).min(mine.len());
        let batch = &mine[from..upto];
        found.clear();
        found.resize(batch.len(), MISS);
        table.probe_these(hashes, keys, batch, &mut found, &mut walk);
        // The rows the batch could not settle, in row order, which is the order they have to go in:
        // two rows of one batch can be the first two rows of one key, and the second only finds the
        // first if the first went in before it was asked.
        for &place in walk.pending() {
            let row = batch[place];
            match table.probe(hashes[row], keys, row) {
                Probe::Found(slot) => found[place] = slot,
                Probe::Vacant(bucket) => {
                    let slot = table.insert(bucket, hashes[row], keys, row)?;
                    debug_assert_eq!(slot, head.len(), "a slot is the number of keys before it");
                    head.push(NONE);
                    tail.push(NONE);
                    found[place] = slot;
                }
            }
        }
        // In row order and after the whole batch has a slot, because the batched pass fills the
        // rows that were already keys and the loop above fills the rest, and a chain that was
        // appended to in that order would hold a key's rows in neither the order they arrived in
        // nor any other one.
        for (&row, &slot) in batch.iter().zip(&found) {
            let at = u32::try_from(row).map_err(|_| too_many_rows())?;
            if tail[slot] == NONE {
                head[slot] = at;
            } else {
                next[tail[slot] as usize].store(at, Ordering::Relaxed);
            }
            tail[slot] = at;
            kept += 1;
        }
        from = upto;
    }
    Ok((table, head, kept))
}

/// The keyed rows of the side sorted into their partitions, in row order inside each one.
///
/// What comes back is one run of rows and where each partition's rows start in it, with one more
/// start at the end. Two passes over the hashes whatever the number of partitions: one counts the
/// rows each partition gets and one puts every row where its partition starts plus the rows of it
/// seen so far. Before this every partition read the whole run of hashes to find its own, which on
/// sixteen threads is sixteen passes, and on q09 at SF1 that was a tenth of the query's CPU.
fn deal_rows(hashes: &[u64], keyed: &[bool], bits: u32, count: usize) -> (Vec<usize>, Vec<usize>) {
    let mut starts = vec![0; count + 1];
    for (row, &hash) in hashes.iter().enumerate() {
        if keyed[row] {
            starts[part_of(hash, bits) + 1] += 1;
        }
    }
    for part in 0..count {
        starts[part + 1] += starts[part];
    }
    let mut at = starts.clone();
    let mut dealt = vec![0; starts[count]];
    for (row, &hash) in hashes.iter().enumerate() {
        if keyed[row] {
            let part = part_of(hash, bits);
            dealt[at[part]] = row;
            at[part] += 1;
        }
    }
    (starts, dealt)
}

/// How many of a hash's top bits name a partition, which is none below [`SPLIT`] rows.
///
/// A power of two of them, and no more than the threads there are to run them on, because a
/// partition nobody is free to take is a pass over the hashes that bought nothing.
fn split_into(rows: usize, threads: usize) -> u32 {
    if rows < SPLIT || threads <= 1 {
        return 0;
    }
    threads.next_power_of_two().trailing_zeros()
}

/// Which partition a hash belongs to.
fn part_of(hash: u64, bits: u32) -> usize {
    if bits == 0 {
        return 0;
    }
    (hash >> (64 - bits)) as usize
}

/// The buffers one instance of a probe walks a driving chunk with.
///
/// Held by the instance and reused, so a chunk costs the allocator nothing after the first one.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    hashes: Vec<u64>,
    keyed: Vec<bool>,
    walk: Walk,
    /// The chunk's rows dealt into the partitions they belong to, one list per partition.
    by_part: Vec<Vec<usize>>,
    /// What one batch of one of those lists found, by place in the batch.
    found: Vec<usize>,
    /// The chunk's keys widened, for the direct form.
    block: Vec<i64>,
}

/// Whether a key of this type is a signed integer the direct form can use as a place.
fn integer(logical: &LogicalType) -> bool {
    matches!(
        logical,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    )
}

/// Which rows have a key at all, which is every row until a rejected null says otherwise.
///
/// Column at a time, and only the columns that can reject one, so a join on columns that are not
/// nullable costs a look at each column's mask and no pass over the rows at all.
fn which_are_keyed(keys: &[Vector], rows: usize, nulls: &[bool], keyed: &mut Vec<bool>) {
    keyed.clear();
    keyed.resize(rows, true);
    for (column, &stored) in keys.iter().zip(nulls) {
        if stored || !has_nulls(column, rows) {
            continue;
        }
        for (row, flag) in keyed.iter_mut().enumerate().take(rows) {
            *flag = *flag && !column.is_null_at(row);
        }
    }
}

/// Whether a column could hold a null at all, which is the mask for most forms and not for two.
///
/// A dictionary and a run length vector keep their nulls in the values they point at and are built
/// with every row marked present in the mask beside them, so asking the mask about one of those
/// gets a confident no about a column that is full of nulls. [`Vector::is_null_at`] is the one that
/// reads through, and this is only here to say when the pass that calls it can be skipped.
pub(crate) fn has_nulls(column: &Vector, rows: usize) -> bool {
    match column.form() {
        Form::Dictionary | Form::Rle => true,
        _ => column.validity().has_nulls(rows),
    }
}

/// What a gathered side too long to thread a chain through says.
fn too_many_rows() -> Error {
    Error::out_of_memory(format!(
        "a hash join cannot gather more than {} rows on one side",
        NONE - 1
    ))
}

/// A row of values, which is what the tests below build their key columns out of.
#[cfg(test)]
fn column(values: &[Option<i32>]) -> Vector {
    use rudb_common::Value;
    let values: Vec<Value> =
        values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
    Vector::from_values(LogicalType::Integer, &values).expect("a column of integers")
}

#[cfg(test)]
mod tests {
    use rudb_common::Cancel;
    use rudb_pipeline::{Lease, Pool};

    use super::{Lookup, MISS, SPLIT, Scratch, column, deal_rows, part_of, split_into};

    /// Every keyed row lands in the partition its hash names, once, in row order, and a row that
    /// is not keyed lands nowhere.
    #[test]
    fn rows_are_dealt_to_the_partition_their_hash_names_in_row_order() {
        let hashes: Vec<u64> =
            (0..40_u64).map(|row| row.wrapping_mul(0x9E37_79B9_7F4A_7C15)).collect();
        let keyed: Vec<bool> = (0..40).map(|row| row % 5 != 0).collect();
        let (starts, dealt) = deal_rows(&hashes, &keyed, 2, 4);
        assert_eq!(starts.len(), 5);
        assert_eq!(dealt.len(), 32, "the eight rows that are not keyed are left out");
        for part in 0..4 {
            let mine = &dealt[starts[part]..starts[part + 1]];
            let expected: Vec<usize> =
                (0..40).filter(|&row| keyed[row] && part_of(hashes[row], 2) == part).collect();
            assert_eq!(mine, expected);
        }
    }

    /// Builds a lookup over one integer key column on one thread, nulls rejected.
    fn built(values: &[Option<i32>]) -> Lookup {
        built_by(values, &[false], &Lease::alone())
    }

    /// The same, saying what a null means and how many threads may work on it.
    fn built_by(values: &[Option<i32>], nulls: &[bool], threads: &Lease<'_>) -> Lookup {
        Lookup::build(&[column(values)], values.len(), nulls, threads, &Cancel::new())
            .expect("a build")
    }

    /// What one driving row of the same shape finds, in the order it finds it.
    fn found(lookup: &Lookup, values: &[Option<i32>]) -> Vec<Vec<u32>> {
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        lookup.slots(&[column(values)], values.len(), &[false], &mut scratch, &mut slots);
        let mut chain = Vec::new();
        slots
            .iter()
            .map(|&slot| {
                lookup.matches(slot, &mut chain);
                chain.clone()
            })
            .collect()
    }

    #[test]
    fn a_key_with_no_rows_is_a_miss_and_a_key_with_one_is_that_row() {
        let lookup = built(&[Some(10), Some(20)]);
        assert_eq!(
            found(&lookup, &[Some(20), Some(30), Some(10)]),
            vec![vec![1], Vec::new(), vec![0]]
        );
    }

    /// The order a chain comes out in is the order the gathered side holds the rows, which is what
    /// the nested loop this replaces produced and what keeps a failing test a diff.
    #[test]
    fn a_keys_rows_come_out_in_the_order_the_gathered_side_holds_them() {
        let lookup = built(&[Some(7), Some(9), Some(7), Some(7), Some(9)]);
        assert_eq!(found(&lookup, &[Some(7), Some(9)]), vec![vec![0, 2, 3], vec![1, 4]]);
    }

    /// Two rows of one key in one batch, where the second only finds the first if the first went in
    /// before it was asked. A batch is settled together, so this is the case that says the rows the
    /// batch could not settle are finished in row order.
    #[test]
    fn a_key_first_seen_twice_inside_one_batch_is_one_key() {
        let lookup = built(&[Some(4), Some(4)]);
        assert_eq!(found(&lookup, &[Some(4)]), vec![vec![0, 1]]);
    }

    /// Past one batch, so that the chain is appended to across several of them and the rows of a key
    /// that spans two batches stay in order.
    #[test]
    fn a_chain_that_spans_several_batches_stays_in_order() {
        let values: Vec<Option<i32>> = (0..500).map(|row| Some(row % 3)).collect();
        let lookup = built(&values);
        let mut chain = Vec::new();
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        lookup.slots(&[column(&[Some(1)])], 1, &[false], &mut scratch, &mut slots);
        lookup.matches(slots[0], &mut chain);
        let wanted: Vec<u32> = (0..500).filter(|row| row % 3 == 1).collect();
        assert_eq!(chain, wanted);
    }

    /// `NULL = NULL` is null and not true, so a null key is not stored and not looked up.
    #[test]
    fn a_rejected_null_is_neither_stored_nor_found() {
        let lookup = built(&[Some(1), None, Some(2)]);
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        let driving = [Some(1), None];
        lookup.slots(&[column(&driving)], 2, &[false], &mut scratch, &mut slots);
        assert_ne!(slots[0], MISS, "a driving row with a key finds it");
        assert_eq!(slots[1], MISS, "a driving row whose key is null finds nothing");
    }

    /// `IS NOT DISTINCT FROM` is the other rule for the same value, and the table has always been
    /// able to hold it because a group by puts every null in one group.
    #[test]
    fn a_null_a_join_calls_a_value_is_stored_and_found() {
        let lookup = built_by(&[Some(1), None, None], &[true], &Lease::alone());
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        let driving = [None];
        lookup.slots(&[column(&driving)], 1, &[true], &mut scratch, &mut slots);
        let mut chain = Vec::new();
        lookup.matches(slots[0], &mut chain);
        assert_eq!(chain, vec![1, 2]);
    }

    #[test]
    fn a_gathered_side_with_nothing_keyed_in_it_is_empty() {
        let nothing = Lookup::build(&[], 0, &[false], &Lease::alone(), &Cancel::new())
            .expect("no columns at all");
        assert!(nothing.is_empty());
        assert!(built(&[None, None]).is_empty(), "every row's key was a rejected null");
        assert!(!built(&[Some(1)]).is_empty());
    }

    /// The split is on the top bits of the hash and the bucket a key lands in is the low bits, so
    /// a partition's own table has to see the whole spread of buckets. Splitting the other way
    /// round would leave every row of a partition agreeing on the low bits of its bucket, which is
    /// one chain per partition and a table that is a list.
    #[test]
    fn a_partition_is_named_by_the_top_bits_and_a_bucket_by_the_low_ones() {
        assert_eq!(part_of(0, 2), 0);
        assert_eq!(part_of(u64::MAX, 2), 3);
        assert_eq!(part_of(1 << 62, 2), 1);
        assert_eq!(part_of(u64::MAX, 0), 0, "one partition holds everything");
        assert_eq!(part_of(0xFFFF_FFFF, 2), 0, "the low bits say nothing about which partition");
    }

    /// A side small enough that the dealing would cost more than the building saves stays on one
    /// thread, and so does a lease with nothing to spend.
    #[test]
    fn a_small_side_is_not_split_at_all() {
        assert_eq!(split_into(1_000, 8), 0);
        assert_eq!(split_into(SPLIT - 1, 8), 0);
        assert_eq!(split_into(SPLIT, 1), 0);
        assert_eq!(split_into(SPLIT, 8), 3);
        assert_eq!(split_into(SPLIT, 6), 3, "rounded up to a power of two");
    }

    /// Every key's rows, driven by the keys `0..9` times `spread`, in the order the side holds them,
    /// and a miss for a key the side does not hold.
    fn answers_in_order(lookup: &Lookup, values: &[Option<i32>], spread: i32) {
        let mut scratch = Scratch::default();
        let mut slots = Vec::new();
        let driving: Vec<Option<i32>> = (-1..9).map(|key| Some(key * spread)).collect();
        lookup.slots(&[column(&driving)], driving.len(), &[false], &mut scratch, &mut slots);
        let mut chain = Vec::new();
        for (&key, &slot) in driving.iter().zip(&slots) {
            let wanted: Vec<u32> = (0..values.len())
                .filter(|&row| values[row] == key)
                .map(|row| u32::try_from(row).expect("a side this long"))
                .collect();
            if wanted.is_empty() {
                assert_eq!(slot, MISS, "key {key:?} is not in the side");
                continue;
            }
            lookup.matches(slot, &mut chain);
            assert_eq!(chain, wanted, "key {key:?} came out in the wrong order");
        }
    }

    /// The one that matters, over enough rows to be split for real. Every key's rows still come out
    /// in the order the side holds them, which is the thing several threads filling several tables
    /// could break and the thing a failing join test would show as a reordered diff. The keys are
    /// spread far apart so that the side takes the table rather than the direct form.
    #[test]
    fn a_side_built_in_partitions_answers_the_same_as_one_built_whole() {
        let spread = 1_000_003;
        let values: Vec<Option<i32>> =
            (0..SPLIT as i32 + 1_000).map(|row| Some(row % 7 * spread)).collect();
        let pool = Pool::new(4);
        let lookup = built_by(&values, &[false], &pool.lease(4));
        assert!(lookup.low.is_none(), "keys this far apart take the table");
        assert!(lookup.parts.len() > 1, "a side this long is split");
        answers_in_order(&lookup, &values, spread);
    }

    /// The direct form over enough rows to be split, with repeats, gaps and nulls. Each partition
    /// owns a run of places, and a key's rows still come out in the order the side holds them.
    #[test]
    fn a_direct_side_built_in_partitions_answers_in_order() {
        let values: Vec<Option<i32>> = (0..SPLIT as i32 + 1_000)
            .map(|row| (row % 11 != 5).then_some(row % 13 % 9))
            .filter(|value| *value != Some(4))
            .collect();
        let pool = Pool::new(4);
        let lookup = built_by(&values, &[false], &pool.lease(4));
        assert_eq!(lookup.low, Some(0), "keys this close index the head");
        assert!(lookup.parts.is_empty());
        answers_in_order(&lookup, &values, 1);
        assert!(!lookup.single(), "each key has many rows");
    }

    /// One row a key, the shape of a join against a primary key, starting away from zero. Every key
    /// is its row, a key just past either end is a miss, and the table says each chain is one long.
    #[test]
    fn a_direct_side_of_distinct_keys_is_single_and_misses_past_its_ends() {
        let values: Vec<Option<i32>> = (0..100).map(|row| Some(1_000 - row * 2)).collect();
        let lookup = built(&values);
        assert_eq!(lookup.low, Some(802));
        assert!(lookup.single());
        let found = found(&lookup, &[Some(1_000), Some(802), Some(801), Some(1_002), Some(999)]);
        assert_eq!(found, [vec![0], vec![99], vec![], vec![], vec![]]);
    }
}
