//! Fixed-width radix ownership for grouped `COUNT(DISTINCT BIGINT)` with a TopN parent.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Spent, Stage, Value, stage};
use rudb_vector::{Chunk, Vector};

use crate::key::{mix, spread};
use crate::rows;
use crate::signed::SignedReader;

const PARTITIONS: usize = 16;
const EMPTY: u32 = u32::MAX;
const NOTHING: u64 = 0x9e37_79b9_7f4a_7c15;

#[derive(Debug)]
pub(crate) struct Exchange {
    partitions: Vec<Mutex<Held>>,
    held: Mutex<Vec<Reservation>>,
}

/// One radix partition's rows, as the run each instance handed over rather than one flat run.
///
/// An instance that finishes used to append its run onto the shared one, which copies every record
/// it holds while holding that partition's lock. With sixteen instances that is fifteen sixteenths
/// of the whole column copied, sixteen bytes a row, serialised behind sixteen locks. Handing the run
/// over is a move instead, and the pass that deduplicates the pairs walks the runs one after another
/// and cannot tell the difference.
#[derive(Debug, Default)]
struct Held {
    runs: Vec<Run>,
}

impl Held {
    fn rows(&self) -> usize {
        self.runs.iter().map(|run| run.rows.len()).sum()
    }
}

#[derive(Debug, Clone, Copy)]
struct Record {
    user: i64,
    group: i32,
    pair_hash: u32,
}

fn group_hash(row: Record, valid: bool) -> u32 {
    let word = if valid { i64::from(row.group) as u64 } else { NOTHING };
    let wide = spread(mix(0, word));
    (wide ^ (wide >> 32)) as u32
}

/// One instance's rows for one radix partition.
#[derive(Debug, Default)]
struct Run {
    rows: Vec<Record>,
    /// Empty while every group key in this run is valid.
    validity: Vec<bool>,
}

impl Run {
    fn push(&mut self, row: Record, valid: bool) {
        if !valid && self.validity.is_empty() {
            self.validity.resize(self.rows.len(), true);
        }
        self.rows.push(row);
        if !self.validity.is_empty() {
            self.validity.push(valid);
        }
    }

    fn valid_at(&self, row: usize) -> bool {
        self.validity.is_empty() || self.validity[row]
    }

    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<Record>() + self.validity.capacity() * size_of::<bool>()
    }
}

#[derive(Debug)]
pub(crate) struct Local {
    used: bool,
    partitions: Vec<Run>,
    memory: Reservation,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            partitions: (0..PARTITIONS).map(|_| Run::default()).collect(),
            memory: memory.reservation(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
    }
}

impl Exchange {
    /// Buffers one chunk when its group representation can remain fixed width.
    pub(crate) fn buffer(
        slot: &OnceLock<Self>,
        group: &Vector,
        user: &Vector,
        rows: usize,
        local: &mut Local,
    ) -> Result<bool> {
        slot.get_or_init(|| Self {
            partitions: (0..PARTITIONS).map(|_| Mutex::new(Held::default())).collect(),
            held: Mutex::new(Vec::new()),
        });
        let before = local.partitions.iter().map(Run::footprint).sum::<usize>();
        let shift = u32::BITS - PARTITIONS.ilog2();
        let all_valid = !group.validity().has_nulls(rows) && !user.validity().has_nulls(rows);
        if all_valid {
            // Neither column has a null, so the layout is the only thing that changes between rows
            // and it is picked once here rather than once a row. See `SignedReader`.
            let group = SignedReader::new(group);
            let user = SignedReader::new(user);
            for row in 0..rows {
                let group = group.at(row) as i32;
                scatter(&mut local.partitions, shift, group, true, user.at(row) as i64);
            }
        } else {
            for row in 0..rows {
                if user.is_null_at(row) {
                    continue;
                }
                let user = i64::try_from(user.signed_at(row).ok_or_else(|| {
                    Error::internal("a distinct BIGINT value has no signed representation")
                })?)
                .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?;
                let valid = !group.is_null_at(row);
                let group = if !valid {
                    0
                } else {
                    i32::try_from(group.signed_at(row).ok_or_else(|| {
                        Error::internal("an INTEGER group has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("an INTEGER group is out of range"))?
                };
                scatter(&mut local.partitions, shift, group, valid, user);
            }
        }
        let after = local.partitions.iter().map(Run::footprint).sum::<usize>();
        local.memory.grow(width(after.saturating_sub(before)))?;
        local.used = true;
        Ok(true)
    }

    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        for (at, run) in local.partitions.iter_mut().enumerate() {
            if !run.rows.is_empty() {
                let run = std::mem::take(run);
                self.partitions[at].lock().map_err(poisoned)?.runs.push(run);
            }
        }
        self.held.lock().map_err(poisoned)?.push(local.memory);
        Ok(())
    }

    pub(crate) fn finish(&self, bound: usize, memory: &Memory) -> Result<Vec<Chunk>> {
        let input = self
            .partitions
            .iter()
            .map(|partition| partition.lock().map(|held| held.rows()).map_err(poisoned))
            .sum::<Result<usize>>()?;
        let degree = input.div_ceil(65_536).clamp(1, PARTITIONS);
        let next = AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<Result<Output>>>> =
            (0..PARTITIONS).map(|_| Mutex::new(None)).collect();
        let outputs = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(degree - 1);
            for _ in 1..degree {
                handles.push(scope.spawn(|| {
                    self.finish_next(&next, &slots, bound, memory);
                    stage::here()
                }));
            }
            self.finish_next(&next, &slots, bound, memory);
            let mut theirs = Spent::none();
            for handle in handles {
                let spent = handle
                    .join()
                    .map_err(|_| Error::internal("a grouped distinct radix worker panicked"))?;
                theirs.add(spent);
            }
            stage::gained(theirs);
            let mut outputs = Vec::with_capacity(PARTITIONS);
            for (at, slot) in slots.iter().enumerate() {
                outputs.push(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!(
                        "nothing finished grouped distinct radix partition {at}"
                    )))
                })?);
            }
            Ok::<_, Error>(outputs)
        })?;
        let mut chunks = Vec::new();
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        for Output { chunks: mut part, held: charge } in outputs {
            chunks.append(&mut part);
            held.push(charge);
        }
        Ok(chunks)
    }

    fn finish_next(
        &self,
        next: &AtomicUsize,
        slots: &[Mutex<Option<Result<Output>>>],
        bound: usize,
        memory: &Memory,
    ) {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(partition) = self.partitions.get(at) else { return };
            let done = partition
                .lock()
                .map_err(poisoned)
                .and_then(|mut rows| finish_partition(&mut rows, bound, memory));
            if let Ok(mut slot) = slots[at].lock() {
                *slot = Some(done);
            }
        }
    }
}

struct Output {
    chunks: Vec<Chunk>,
    held: Reservation,
}

fn finish_partition(partition: &mut Held, bound: usize, memory: &Memory) -> Result<Output> {
    let held_rows = partition.rows();
    let pair_capacity = held_rows.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width(pair_capacity * size_of::<u32>()))?;
    let mut pair_buckets = vec![EMPTY; pair_capacity];
    let pair_mask = pair_capacity - 1;
    let all_valid = partition.runs.iter().all(|run| run.validity.is_empty());
    // The deduplicated pairs used to be compacted into the front of the one run the partition held.
    // There is no one run to compact into now, so they are collected here instead, and this is where
    // they are read from for the rest of the pass. It is one record per distinct pair rather than one
    // per row, which is the same bound the compaction had.
    let mut unique: Vec<Record> = Vec::new();
    let mut unique_validity: Vec<bool> = Vec::new();
    let timing = stage::Timing::start(Stage::Fold);
    for run in &partition.runs {
        for (source, &row) in run.rows.iter().enumerate() {
            let valid = all_valid || run.valid_at(source);
            let mut at = row.pair_hash as usize & pair_mask;
            loop {
                let slot = pair_buckets[at];
                if slot == EMPTY {
                    pair_buckets[at] = u32::try_from(unique.len()).map_err(|_| {
                        Error::out_of_memory("a grouped distinct radix partition is too large")
                    })?;
                    unique.push(row);
                    if !all_valid {
                        unique_validity.push(valid);
                    }
                    break;
                }
                let slot = slot as usize;
                let held = unique[slot];
                let held_valid = all_valid || unique_validity[slot];
                if held.pair_hash == row.pair_hash
                    && held.group == row.group
                    && held.user == row.user
                    && held_valid == valid
                {
                    break;
                }
                at = (at + 1) & pair_mask;
            }
        }
    }
    let pairs = unique.len();
    // The rows themselves are not read again, only the distinct pairs, so give the memory back
    // before the group pass rather than at the end of the query.
    partition.runs.clear();
    working.grow(width(
        unique.capacity() * size_of::<Record>() + unique_validity.capacity() * size_of::<bool>(),
    ))?;
    let partition = Run { rows: unique, validity: unique_validity };

    let mut group_buckets = vec![EMPTY; 64];
    let mut group_rows: Vec<usize> = Vec::with_capacity(32);
    let mut counts: Vec<i64> = Vec::with_capacity(32);
    working.grow(width(
        group_buckets.capacity() * size_of::<u32>()
            + group_rows.capacity() * size_of::<usize>()
            + counts.capacity() * size_of::<i64>(),
    ))?;
    for row in 0..pairs {
        if (group_rows.len() + 1) * 2 > group_buckets.len() {
            let old = group_buckets.len();
            let new = old * 2;
            working.grow(width((new - old) * size_of::<u32>()))?;
            let mut grown = vec![EMPTY; new];
            let mask = new - 1;
            for (slot, &source) in group_rows.iter().enumerate() {
                let valid = all_valid || partition.validity[source];
                let mut at = group_hash(partition.rows[source], valid) as usize & mask;
                while grown[at] != EMPTY {
                    at = (at + 1) & mask;
                }
                grown[at] = slot as u32;
            }
            group_buckets = grown;
        }
        if group_rows.len() == group_rows.capacity() {
            let old = group_rows.capacity();
            let new = old.max(1) * 2;
            working.grow(width((new - old) * (size_of::<usize>() + size_of::<i64>())))?;
            group_rows.reserve_exact(new - old);
            counts.reserve_exact(new - old);
        }
        let record = partition.rows[row];
        let valid = all_valid || partition.validity[row];
        let mask = group_buckets.len() - 1;
        let hash = group_hash(record, valid);
        let mut at = hash as usize & mask;
        let slot = loop {
            let slot = group_buckets[at];
            if slot == EMPTY {
                let slot = group_rows.len();
                group_buckets[at] = slot as u32;
                group_rows.push(row);
                counts.push(0);
                break slot;
            }
            let slot = slot as usize;
            let held_row = group_rows[slot];
            let held = partition.rows[held_row];
            let held_valid = all_valid || partition.validity[held_row];
            if group_hash(held, held_valid) == hash
                && held.group == record.group
                && held_valid == valid
            {
                break slot;
            }
            at = (at + 1) & mask;
        };
        counts[slot] = counts[slot]
            .checked_add(1)
            .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
    }
    timing.stop(0);

    let timing = stage::Timing::start(Stage::Emit);
    let mut best = Vec::with_capacity(bound.min(counts.len()));
    for slot in 0..counts.len() {
        let at = best.partition_point(|&kept| counts[kept] >= counts[slot]);
        if at < bound {
            best.insert(at, slot);
            best.truncate(bound);
        }
    }
    best.sort_unstable();
    let mut output = Vec::with_capacity(best.len());
    for slot in best {
        let source = group_rows[slot];
        let row = partition.rows[source];
        let valid = all_valid || partition.validity[source];
        let group = if !valid { Value::Null } else { Value::Integer(row.group) };
        output.push(vec![group, Value::BigInt(counts[slot])]);
    }
    let mut held = memory.reservation();
    let chunks = rows::chunks(&[LogicalType::Integer, LogicalType::BigInt], &output, &mut held)?;
    timing.stop(0);
    Ok(Output { chunks, held })
}

/// One row into the radix partition its group hash picks.
///
/// Pulled out of [`Exchange::buffer`] so that the loop that reads both columns where they lie and the
/// loop that asks the vectors a row at a time cannot drift apart on which partition a row belongs in
/// or on what its hashes are.
///
/// The shift leaves exactly the bits that index [`PARTITIONS`] of them, so the index is always in
/// range and the bounds check never fires.
#[inline]
fn scatter(partitions: &mut [Run], shift: u32, group: i32, valid: bool, user: i64) {
    let group_word = if valid { i64::from(group) as u64 } else { NOTHING };
    let wide_group = spread(mix(0, group_word));
    let wide_pair = spread(mix(wide_group, user as u64));
    let group_hash = (wide_group ^ (wide_group >> 32)) as u32;
    let pair_hash = (wide_pair ^ (wide_pair >> 32)) as u32;
    partitions[(group_hash >> shift) as usize].push(Record { user, group, pair_hash }, valid);
}

fn width(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a grouped distinct radix lock was poisoned")
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use rudb_common::{Memory, Value};

    use super::{Held, Record, Run, finish_partition};

    #[test]
    fn one_partition_deduplicates_pairs_and_counts_groups_across_the_runs_it_was_handed() {
        let row = |group, user, pair_hash| Record { user, group, pair_hash };
        // Three instances, and the pair (3, 10) arrives in two of them, which is the case the
        // deduplication has to see across a run boundary rather than only within one run.
        let mut first = Run::default();
        first.push(row(3, 10, 5), true);
        first.push(row(3, 10, 5), true);
        let mut second = Run::default();
        second.push(row(3, 11, 5), true);
        second.push(row(3, 10, 5), true);
        let mut third = Run::default();
        third.push(row(4, 10, 5), true);
        third.push(row(0, 10, 5), false);
        let mut partition = Held { runs: vec![first, Run::default(), second, third] };
        let output = finish_partition(&mut partition, 10, &Memory::unlimited())
            .expect("a grouped distinct partition");
        let mut rows = Vec::new();
        for chunk in output.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row: &Vec<Value>| format!("{row:?}"));
        assert_eq!(
            rows,
            [
                vec![Value::Integer(3), Value::BigInt(2)],
                vec![Value::Integer(4), Value::BigInt(1)],
                vec![Value::Null, Value::BigInt(1)],
            ]
        );
        assert_eq!(size_of::<Record>(), 16);
    }
}
