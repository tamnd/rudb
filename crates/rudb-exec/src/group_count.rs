//! Radix ownership for `COUNT(*)` grouped by one signed integer and cut down by a TopN.
//!
//! `GROUP BY UserID ORDER BY COUNT(*) DESC LIMIT 10` is the query this is for. It has millions of
//! groups, most of them seen a handful of times, and the general table spent its time probing and
//! growing a table of boxed keys and per group states that did not fit in any cache. All the query
//! needs per group is one number, so the rows travel as an eight byte key and a four byte weight
//! and are counted in a flat table of keys once every instance is done.
//!
//! The weight is how many rows in a row had the same key. The ClickBench file is sorted on the
//! counter, the date and the user, so a user's rows mostly sit together, and an instance hands on
//! one record for a run of them rather than one a row. A file in no order at all gets a weight of
//! one on every record and pays four bytes a row for it.
//!
//! The finish splits each radix partition again by more bits of the hash until a split's table sits
//! in the core's own cache, for the reason [`crate::group`]'s fixed exchange does, and keeps only the
//! `bound` largest groups of each split. Nothing outside a split can change a split's counts, so the
//! largest overall are among what the splits keep.

use std::mem::size_of;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, Value, stage};
use rudb_pipeline::Lease;
use rudb_vector::{Chunk, Vector};

use crate::group::{largest, signed_value};
use crate::key::{mix, spread};
use crate::pairs::{PARTITIONS, in_parallel};
use crate::rows;
use crate::signed::SignedBlock;

/// How many records one split of a partition is sized to hold. See [`crate::group`]'s
/// `FIXED_SPLIT_ROWS`, which this is the same number as for a record of about the same size.
const SPLIT_ROWS: usize = 16_384;

/// How far a key's hash is shifted to pick its radix partition out of the top bits.
const SHIFT: u32 = u64::BITS - PARTITIONS.ilog2();

/// The bucket value that says a bucket is free.
const EMPTY: u32 = u32::MAX;

#[inline]
fn hash(key: i64) -> u64 {
    spread(mix(0, key as u64))
}

/// Which split of `splits` a key belongs to, by bits of its hash that neither the radix partition
/// nor a table of a split's size reads.
#[inline]
fn split(hash: u64, splits: usize) -> usize {
    (hash >> 40) as usize & (splits - 1)
}

/// Keys and how many rows each stands for, one run per instance and partition.
#[derive(Debug, Default)]
struct Run {
    keys: Vec<i64>,
    weights: Vec<u32>,
}

impl Run {
    fn push(&mut self, key: i64, weight: u32) {
        self.keys.push(key);
        self.weights.push(weight);
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn footprint(&self) -> usize {
        self.keys.capacity() * size_of::<i64>() + self.weights.capacity() * size_of::<u32>()
    }
}

#[derive(Debug)]
pub(crate) struct Exchange {
    key: LogicalType,
    partitions: Vec<Mutex<Vec<Run>>>,
    /// The rows whose key was null, which are one group and never scattered.
    nulls: AtomicI64,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug)]
pub(crate) struct Local {
    used: bool,
    runs: Vec<Run>,
    /// The key of the run of rows still being counted and how many rows it has so far.
    pending: Option<(i64, u32)>,
    nulls: i64,
    block: SignedBlock,
    memory: Reservation,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            runs: (0..PARTITIONS).map(|_| Run::default()).collect(),
            pending: None,
            nulls: 0,
            block: SignedBlock::default(),
            memory: memory.reservation(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
    }

    fn footprint(&self) -> usize {
        self.runs.iter().map(Run::footprint).sum()
    }

    #[inline]
    fn scatter(&mut self, key: i64, weight: u32) {
        self.runs[(hash(key) >> SHIFT) as usize].push(key, weight);
    }
}

impl Exchange {
    /// Takes one chunk's keys into this instance's runs.
    pub(crate) fn buffer(
        slot: &OnceLock<Self>,
        key_type: &LogicalType,
        key: &Vector,
        rows: usize,
        local: &mut Local,
    ) -> Result<()> {
        slot.get_or_init(|| Self {
            key: key_type.clone(),
            partitions: (0..PARTITIONS).map(|_| Mutex::new(Vec::new())).collect(),
            nulls: AtomicI64::new(0),
            held: Mutex::new(Vec::new()),
        });
        let timing = stage::Timing::start(Stage::Scatter);
        let before = local.footprint();
        // The buffer comes out of the instance for the length of the loop, because reading it borrows
        // it and scattering a record into the instance borrows that again.
        let mut block = std::mem::take(&mut local.block);
        block.read(rows, key)?;
        let nulled = block.nulled();
        let values = block.cut(rows)?;
        let mut pending = local.pending.take();
        for (row, &value) in values.iter().enumerate() {
            if nulled && key.is_null_at(row) {
                local.nulls += 1;
                continue;
            }
            pending = match pending {
                Some((held, weight)) if held == value && weight < u32::MAX => {
                    Some((held, weight + 1))
                }
                Some((held, weight)) => {
                    local.scatter(held, weight);
                    Some((value, 1))
                }
                None => Some((value, 1)),
            };
        }
        local.pending = pending;
        local.block = block;
        local.memory.grow(width(local.footprint().saturating_sub(before)))?;
        local.used = true;
        timing.stop(0);
        Ok(())
    }

    /// Hands one instance's runs over, by move.
    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        if let Some((key, weight)) = local.pending.take() {
            local.scatter(key, weight);
        }
        self.nulls.fetch_add(local.nulls, Ordering::Relaxed);
        for (at, run) in local.runs.iter_mut().enumerate() {
            if run.len() != 0 {
                let run = std::mem::take(run);
                self.partitions[at].lock().map_err(poisoned)?.push(run);
            }
        }
        self.held.lock().map_err(poisoned)?.push(local.memory);
        Ok(())
    }

    /// How many records every instance handed over, which is what the finish is sized by.
    pub(crate) fn records(&self) -> Result<usize> {
        self.partitions
            .iter()
            .map(|runs| {
                runs.lock().map(|runs| runs.iter().map(Run::len).sum::<usize>()).map_err(poisoned)
            })
            .sum()
    }

    /// Counts every partition on `degree` threads and keeps the `bound` largest groups of each split.
    pub(crate) fn finish(
        &self,
        threads: &Lease<'_>,
        degree: usize,
        bound: usize,
        memory: &Memory,
    ) -> Result<Vec<Chunk>> {
        let parts = in_parallel(threads, PARTITIONS, degree, "counted radix partition", |at| {
            let runs = std::mem::take(&mut *self.partitions[at].lock().map_err(poisoned)?);
            count_partition(runs, bound, memory)
        })?;
        let timing = stage::Timing::start(Stage::Emit);
        let nulls = self.nulls.load(Ordering::Relaxed);
        let mut output = Vec::new();
        if nulls > 0 {
            output.push(vec![Value::Null, Value::BigInt(nulls)]);
        }
        for part in parts {
            for (key, count) in part {
                output.push(vec![signed_value(&self.key, key)?, Value::BigInt(count)]);
            }
        }
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        let mut charge = memory.reservation();
        let chunks = rows::chunks(&[self.key.clone(), LogicalType::BigInt], &output, &mut charge)?;
        held.push(charge);
        timing.stop(0);
        Ok(chunks)
    }
}

/// One partition's groups and their counts, cut to the `bound` largest of each split.
fn count_partition(runs: Vec<Run>, bound: usize, memory: &Memory) -> Result<Vec<(i64, i64)>> {
    let total: usize = runs.iter().map(Run::len).sum();
    if total == 0 {
        return Ok(Vec::new());
    }
    let reserving = stage::Timing::start(Stage::Reserve);
    let splits = (total / SPLIT_ROWS).max(1).next_power_of_two();
    let share = total.div_ceil(splits);
    let share = (share + share.isqrt() * 4).min(total);
    let capacity = share.saturating_mul(2).max(64).next_power_of_two();
    let mut working = memory.reservation();
    working.grow(width(
        capacity * size_of::<u32>()
            + share * (size_of::<i64>() * 2)
            + if splits > 1 { total * (size_of::<i64>() + size_of::<u32>()) } else { 0 },
    ))?;
    reserving.stop(0);
    let parts = if splits == 1 {
        runs
    } else {
        let timing = stage::Timing::start(Stage::Merge);
        let mut parts: Vec<Run> = (0..splits)
            .map(|_| Run { keys: Vec::with_capacity(share), weights: Vec::with_capacity(share) })
            .collect();
        for run in runs {
            for (&key, &weight) in run.keys.iter().zip(&run.weights) {
                parts[split(hash(key), splits)].push(key, weight);
            }
        }
        timing.stop(0);
        parts
    };
    let timing = stage::Timing::start(Stage::Fold);
    let mut buckets: Vec<u32> = Vec::with_capacity(capacity);
    let mut keys: Vec<i64> = Vec::with_capacity(share);
    let mut counts: Vec<i64> = Vec::with_capacity(share);
    let mut output = Vec::new();
    // Without splitting, the runs of every instance are one split between them and share a table.
    let groups_of = |parts: &[Run]| parts.iter().map(Run::len).sum::<usize>();
    let batches: Vec<Vec<Run>> =
        if splits == 1 { vec![parts] } else { parts.into_iter().map(|part| vec![part]).collect() };
    for batch in batches {
        let rows = groups_of(&batch);
        let size = rows.saturating_mul(2).max(64).next_power_of_two();
        let mask = size - 1;
        buckets.clear();
        buckets.resize(size, EMPTY);
        keys.clear();
        counts.clear();
        for run in &batch {
            for (&key, &weight) in run.keys.iter().zip(&run.weights) {
                let mut at = hash(key) as usize & mask;
                loop {
                    let slot = buckets[at];
                    if slot == EMPTY {
                        buckets[at] = u32::try_from(keys.len())
                            .map_err(|_| Error::internal("a counted radix split is too large"))?;
                        keys.push(key);
                        counts.push(i64::from(weight));
                        break;
                    }
                    if keys[slot as usize] == key {
                        counts[slot as usize] += i64::from(weight);
                        break;
                    }
                    at = (at + 1) & mask;
                }
            }
        }
        for slot in largest(keys.len(), bound, |slot| counts[slot]) {
            output.push((keys[slot], counts[slot]));
        }
    }
    timing.stop(0);
    Ok(output)
}

fn width(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a counted radix exchange lock was poisoned")
}

#[cfg(test)]
mod tests {
    use super::{Run, count_partition};
    use rudb_common::Memory;

    #[test]
    fn a_counted_partition_adds_weights_and_keeps_the_largest() {
        let memory = Memory::unlimited();
        let mut first = Run::default();
        let mut second = Run::default();
        for key in 0..40_000_i64 {
            first.push(key, 1);
        }
        first.push(7, 5);
        second.push(7, 2);
        second.push(-3, 9);
        second.push(40_001, 3);
        let mut kept = count_partition(vec![first, second], 3, &memory).expect("counted");
        kept.sort_by_key(|&(key, count)| (std::cmp::Reverse(count), key));
        assert_eq!(&kept[..3], &[(-3, 9), (7, 8), (40_001, 3)]);
    }
}
