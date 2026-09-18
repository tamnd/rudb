//! Direct radix ownership for a mixed numeric and distinct grouped aggregate.
//!
//! The query this is for asks for both kinds of answer about the same groups, a `SUM` and a `COUNT`
//! and an `AVG` alongside a `COUNT(DISTINCT)`, and the two kinds want the rows partitioned on
//! different things. The numeric side wants them on the group, because a group's running total has
//! to be in one place. The distinct side wants them on the pair of the group and the value being
//! counted, because deduplicating is the expensive half and a lopsided grouping column otherwise
//! hands the whole of the biggest group to one thread.
//!
//! So the rows go both ways. Each instance buffers a numeric record partitioned by the group hash
//! and a pair record partitioned by the pair hash, and at the end the pairs are deduplicated by
//! [`crate::pairs`] and the survivors are counted into the group tables the numeric side already
//! built. A surviving pair carries its group hash, and the split it comes back in is picked by the
//! top bits of that hash, which is the same arithmetic that picked the group's owner, so the split
//! and the owner are the same number and no group has to be looked for anywhere else.
//!
//! What this replaces is a set of every distinct pair held inside each owner. That set was probed
//! once a row and rehashed as it grew, and on the million row ClickBench file the two functions it
//! amounted to were thirty nine percent of the query.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, Value, stage};
use rudb_kernels::Accumulator;
use rudb_vector::{Chunk, Vector};

use crate::pairs::{
    self, Counted, Held, PARTITIONS, Run, distinct_pairs, group_hash, in_parallel, scatter,
};
use crate::rows;
use crate::signed::SignedReader;

const EMPTY: u32 = u32::MAX;
const FLUSH_ROWS: usize = 32_768;

#[derive(Debug)]
pub(crate) struct Exchange {
    owners: Vec<Mutex<Owner>>,
    /// The pairs behind the distinct count, partitioned on the pair and not on the group.
    pairs: Vec<Mutex<Held>>,
    next_start: AtomicUsize,
    held: Mutex<Vec<Reservation>>,
}

/// One row's numeric part, on its way to the owner of its group.
///
/// The value being counted is not in here. It travels the other way, in a pair record, so this is
/// sixteen bytes rather than twenty four and a flush moves a third less.
#[derive(Debug, Clone, Copy)]
struct Record {
    group: i32,
    group_hash: u32,
    sum: i16,
    mean: i16,
    valid: u8,
}

impl Record {
    const GROUP: u8 = 1;
    const SUM: u8 = 1 << 1;
    const MEAN: u8 = 1 << 2;

    fn has(self, flag: u8) -> bool {
        self.valid & flag != 0
    }
}

#[derive(Debug, Default)]
struct Partition {
    rows: Vec<Record>,
}

impl Partition {
    fn footprint(&self) -> usize {
        self.rows.capacity() * size_of::<Record>()
    }
}

#[derive(Debug)]
pub(crate) struct Local {
    used: bool,
    buffered: usize,
    partitions: Vec<Partition>,
    pairs: Vec<Run>,
    memory: Reservation,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            buffered: 0,
            partitions: (0..PARTITIONS).map(|_| Partition::default()).collect(),
            pairs: (0..PARTITIONS).map(|_| Run::default()).collect(),
            memory: memory.reservation(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
    }

    fn footprint(&self) -> usize {
        self.partitions.iter().map(Partition::footprint).sum::<usize>()
            + self.pairs.iter().map(Run::footprint).sum::<usize>()
    }
}

impl Exchange {
    pub(crate) fn buffer(
        slot: &OnceLock<Self>,
        memory: &Memory,
        inputs: [&Vector; 4],
        rows: usize,
        local: &mut Local,
    ) -> Result<()> {
        let [group, sum, mean, user] = inputs;
        let exchange = slot.get_or_init(|| Self {
            owners: (0..PARTITIONS).map(|_| Mutex::new(Owner::new(memory))).collect(),
            pairs: (0..PARTITIONS).map(|_| Mutex::new(Held::default())).collect(),
            next_start: AtomicUsize::new(0),
            held: Mutex::new(Vec::new()),
        });
        let timing = stage::Timing::start(Stage::Scatter);
        let before = local.footprint();
        let shift = pairs::shift();
        let all_valid = inputs.iter().all(|column| !column.validity().has_nulls(rows));
        if all_valid {
            let group = SignedReader::new(group);
            let sum = SignedReader::new(sum);
            let mean = SignedReader::new(mean);
            let user = SignedReader::new(user);
            for row in 0..rows {
                let key = group.at(row) as i32;
                let hash = group_hash(key, true);
                local.partitions[(hash >> shift) as usize].rows.push(Record {
                    group: key,
                    group_hash: hash,
                    sum: sum.at(row) as i16,
                    mean: mean.at(row) as i16,
                    valid: Record::GROUP | Record::SUM | Record::MEAN,
                });
                scatter(&mut local.pairs, shift, key, true, user.at(row) as i64);
            }
        } else {
            for row in 0..rows {
                let mut valid = 0_u8;
                let key = if group.is_null_at(row) {
                    0
                } else {
                    valid |= Record::GROUP;
                    i32::try_from(group.signed_at(row).ok_or_else(|| {
                        Error::internal("an INTEGER group has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("an INTEGER group is out of range"))?
                };
                let sum = if sum.is_null_at(row) {
                    0
                } else {
                    valid |= Record::SUM;
                    i16::try_from(sum.signed_at(row).ok_or_else(|| {
                        Error::internal("a SMALLINT sum value has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("a SMALLINT sum value is out of range"))?
                };
                let mean = if mean.is_null_at(row) {
                    0
                } else {
                    valid |= Record::MEAN;
                    i16::try_from(mean.signed_at(row).ok_or_else(|| {
                        Error::internal("a SMALLINT average value has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("a SMALLINT average value is out of range"))?
                };
                let hash = group_hash(key, valid & Record::GROUP != 0);
                local.partitions[(hash >> shift) as usize].rows.push(Record {
                    group: key,
                    group_hash: hash,
                    sum,
                    mean,
                    valid,
                });
                // A null value counts towards nothing, so it never becomes a pair. The row still
                // counts towards the numeric aggregates above, which is why this is the only part of
                // it that is skipped.
                if !user.is_null_at(row) {
                    let user = i64::try_from(user.signed_at(row).ok_or_else(|| {
                        Error::internal("a distinct BIGINT value has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?;
                    scatter(&mut local.pairs, shift, key, valid & Record::GROUP != 0, user);
                }
            }
        }
        local.memory.grow(width(local.footprint().saturating_sub(before)))?;
        timing.stop(0);
        local.buffered += rows;
        local.used = true;
        if local.buffered >= FLUSH_ROWS {
            exchange.flush(local)?;
        }
        Ok(())
    }

    fn flush(&self, local: &mut Local) -> Result<()> {
        if local.buffered == 0 {
            return Ok(());
        }
        let timing = stage::Timing::start(Stage::Fold);
        let start = self.next_start.fetch_add(1, Ordering::Relaxed) % PARTITIONS;
        let mut waiting = Vec::new();
        for offset in 0..PARTITIONS {
            let at = (start + offset) % PARTITIONS;
            if local.partitions[at].rows.is_empty() {
                continue;
            }
            match self.owners[at].try_lock() {
                Ok(mut owner) => owner.add_all(&mut local.partitions[at].rows)?,
                Err(TryLockError::WouldBlock) => waiting.push(at),
                Err(TryLockError::Poisoned(problem)) => return Err(poisoned(problem)),
            }
        }
        for at in waiting {
            self.owners[at].lock().map_err(poisoned)?.add_all(&mut local.partitions[at].rows)?;
        }
        timing.stop(0);
        local.buffered = 0;
        Ok(())
    }

    /// Hands one instance's work over, the numeric records by fold and the pairs by move.
    ///
    /// The pairs are not flushed along the way the numeric records are. There is nothing to fold
    /// them into until every instance has finished, so flushing them early would only copy them into
    /// a shared vector under a lock, where handing the run over at the end is a move.
    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        self.flush(&mut local)?;
        for (at, run) in local.pairs.iter_mut().enumerate() {
            if !run.rows.is_empty() {
                let run = std::mem::take(run);
                self.pairs[at].lock().map_err(poisoned)?.runs.push(run);
            }
        }
        self.held.lock().map_err(poisoned)?.push(local.memory);
        Ok(())
    }

    /// Deduplicates the pairs a partition at a time, then counts them into the group tables.
    ///
    /// Every split is asked for, unlike a plain grouped distinct which collapses to one when the
    /// query is small enough to finish on one thread. There is no choice here: the split a group
    /// comes back in has to be the owner that already holds its numeric state, and there are
    /// [`PARTITIONS`] of those whatever the query looks like.
    pub(crate) fn finish(&self, bound: usize, memory: &Memory) -> Result<Vec<Chunk>> {
        let input = self
            .pairs
            .iter()
            .map(|partition| partition.lock().map(|held| held.rows()).map_err(poisoned))
            .sum::<Result<usize>>()?;
        let degree = input.div_ceil(16_384).clamp(1, PARTITIONS);
        let counted =
            in_parallel(PARTITIONS, degree, "deduplicated the pairs of radix partition", |at| {
                let mut partition = self.pairs[at].lock().map_err(poisoned)?;
                distinct_pairs(&mut partition, PARTITIONS, memory)
            })?;
        let outputs = in_parallel(PARTITIONS, degree, "finished mixed radix partition", |at| {
            let mut owner = self.owners[at].lock().map_err(poisoned)?;
            owner.count_distinct(&counted, at)?;
            owner.finish(bound, memory)
        })?;
        // The distinct pairs are read for the last time by the pass above, so the room they took goes
        // back here rather than at the end of the query.
        for part in counted {
            drop(part.held);
        }
        let mut chunks = Vec::new();
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        for Output { chunks: mut part, held: charge } in outputs {
            chunks.append(&mut part);
            held.push(charge);
        }
        Ok(chunks)
    }
}

#[derive(Debug, Default)]
struct State {
    group: i32,
    group_valid: bool,
    count: i64,
    sum: i128,
    sum_seen: bool,
    mean: i128,
    mean_count: i64,
    distinct: i64,
}

#[derive(Debug)]
struct Owner {
    buckets: Vec<u32>,
    states: Vec<State>,
    memory: Reservation,
}

impl Owner {
    fn new(memory: &Memory) -> Self {
        Self { buckets: Vec::new(), states: Vec::new(), memory: memory.reservation() }
    }

    fn add_all(&mut self, rows: &mut Vec<Record>) -> Result<()> {
        for row in rows.drain(..) {
            let slot = self.group(row.group, row.group_hash, row.has(Record::GROUP))?;
            let state = &mut self.states[slot];
            state.count = state
                .count
                .checked_add(1)
                .ok_or_else(|| Error::out_of_range("a mixed COUNT overflowed BIGINT"))?;
            if row.has(Record::SUM) {
                state.sum = state
                    .sum
                    .checked_add(i128::from(row.sum))
                    .ok_or_else(|| Error::out_of_range("a mixed SUM overflowed HUGEINT"))?;
                state.sum_seen = true;
            }
            if row.has(Record::MEAN) {
                state.mean = state
                    .mean
                    .checked_add(i128::from(row.mean))
                    .ok_or_else(|| Error::out_of_range("a mixed AVG total overflowed HUGEINT"))?;
                state.mean_count = state
                    .mean_count
                    .checked_add(1)
                    .ok_or_else(|| Error::out_of_range("a mixed AVG count overflowed BIGINT"))?;
            }
        }
        Ok(())
    }

    /// Adds one of every distinct pair to the group it belongs to.
    ///
    /// The group is already here in every case a query can produce, because a row that made a pair
    /// also made a numeric record and the two went to the same owner. Asking for the slot rather
    /// than looking it up keeps that from being an assumption the code depends on.
    fn count_distinct(&mut self, counted: &[Counted], split: usize) -> Result<()> {
        let timing = stage::Timing::start(Stage::Fold);
        for part in counted {
            for pair in &part.splits[split] {
                let slot = self.group(pair.group, pair.group_hash, pair.valid)?;
                let state = &mut self.states[slot];
                state.distinct = state
                    .distinct
                    .checked_add(1)
                    .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
            }
        }
        timing.stop(0);
        Ok(())
    }

    fn group(&mut self, group: i32, hash: u32, valid: bool) -> Result<usize> {
        if self.buckets.is_empty() || (self.states.len() + 1) * 2 > self.buckets.len() {
            self.grow_groups()?;
        }
        if self.states.len() == self.states.capacity() {
            let old = self.states.capacity();
            let new = old.max(16) * 2;
            self.memory.grow(width((new - old) * size_of::<State>()))?;
            self.states.reserve_exact(new - old);
        }
        let mask = self.buckets.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.buckets[at];
            if slot == EMPTY {
                let slot = self.states.len();
                self.buckets[at] = u32::try_from(slot)
                    .map_err(|_| Error::out_of_memory("too many mixed aggregate groups"))?;
                self.states.push(State { group, group_valid: valid, ..State::default() });
                return Ok(slot);
            }
            let slot = slot as usize;
            let held = &self.states[slot];
            if held.group == group && held.group_valid == valid {
                return Ok(slot);
            }
            at = (at + 1) & mask;
        }
    }

    fn grow_groups(&mut self) -> Result<()> {
        let old = self.buckets.len();
        let new = old.max(32) * 2;
        self.memory.grow(width(new * size_of::<u32>()))?;
        let mut grown = vec![EMPTY; new];
        let mask = new - 1;
        for (slot, state) in self.states.iter().enumerate() {
            let hash = group_hash(state.group, state.group_valid);
            let mut at = hash as usize & mask;
            while grown[at] != EMPTY {
                at = (at + 1) & mask;
            }
            grown[at] = slot as u32;
        }
        self.buckets = grown;
        self.memory.shrink(width(old * size_of::<u32>()));
        Ok(())
    }

    fn finish(&mut self, bound: usize, memory: &Memory) -> Result<Output> {
        let timing = stage::Timing::start(Stage::Emit);
        let mut best: Vec<usize> = Vec::with_capacity(bound.min(self.states.len()));
        for slot in 0..self.states.len() {
            let at =
                best.partition_point(|&kept| self.states[kept].count >= self.states[slot].count);
            if at < bound {
                best.insert(at, slot);
                best.truncate(bound);
            }
        }
        let mut output = Vec::with_capacity(best.len());
        for slot in best {
            let state = &self.states[slot];
            let group = if state.group_valid { Value::Integer(state.group) } else { Value::Null };
            output.push(vec![
                group,
                Accumulator::exact_sum(state.sum, state.sum_seen, &LogicalType::HugeInt)
                    .finish()?,
                Value::BigInt(state.count),
                Accumulator::exact_avg(state.mean, state.mean_count, &LogicalType::Double)
                    .finish()?,
                Value::BigInt(state.distinct),
            ]);
        }
        self.buckets.clear();
        self.buckets.shrink_to_fit();
        self.states.clear();
        self.states.shrink_to_fit();
        self.memory.release();
        let mut held = memory.reservation();
        let chunks = rows::chunks(
            &[
                LogicalType::Integer,
                LogicalType::HugeInt,
                LogicalType::BigInt,
                LogicalType::Double,
                LogicalType::BigInt,
            ],
            &output,
            &mut held,
        )?;
        timing.stop(0);
        Ok(Output { chunks, held })
    }
}

struct Output {
    chunks: Vec<Chunk>,
    held: Reservation,
}

fn width(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a mixed radix lock was poisoned")
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use rudb_common::{LogicalType, Memory, Value};
    use rudb_vector::Vector;

    use crate::pairs::{Held, PARTITIONS, Run, distinct_pairs, group_hash, scatter, shift};

    use super::{Owner, Record, SignedReader};

    #[test]
    fn signed_reader_agrees_with_offset_packed_vectors() {
        let values: Vec<Value> =
            (0..256).map(|row| Value::Integer((row * 37 % 127) - 30)).collect();
        let flat = Vector::from_values(LogicalType::Integer, &values).expect("an integer vector");
        let packed = flat.bit_packed().expect("the vector packs");
        assert!(packed.packed_parts().is_some());
        let cut = packed.slice(3, 200).expect("an offset packed vector");
        let reader = SignedReader::new(&cut);
        for row in 0..cut.len() {
            assert_eq!(reader.at(row), cut.signed_at(row).expect("a signed value"));
        }
    }

    #[test]
    fn one_owner_combines_numeric_and_distinct_states() {
        // The same five rows go both ways, as numeric records here and as pairs below, which is what
        // the operator does with them. One split and one owner, because an owner in a real query
        // only ever sees the split its own groups came back in.
        let row = |group, sum, mean, valid| Record {
            group,
            group_hash: group_hash(group, valid & Record::GROUP != 0),
            sum,
            mean,
            valid,
        };
        let all = Record::GROUP | Record::SUM | Record::MEAN;
        let mut input = vec![
            row(3, 2, 4, all),
            row(3, 3, 6, all),
            row(3, 0, 0, Record::GROUP),
            row(4, 7, 8, all),
            row(0, 5, 2, Record::SUM | Record::MEAN),
        ];
        let memory = Memory::unlimited();
        let mut owner = Owner::new(&memory);
        owner.add_all(&mut input).expect("rows enter one owner");

        let mut runs: Vec<Run> = (0..PARTITIONS).map(|_| Run::default()).collect();
        for (group, user, valid) in
            [(3, 10, true), (3, 10, true), (3, 11, true), (4, 10, true), (0, 10, false)]
        {
            scatter(&mut runs, shift(), group, valid, user);
        }
        // Every run in one partition, so that the deduplication sees all five rows. Which run a pair
        // went to does not change what it is, and the pass compares the pair itself.
        let mut pairs = Held { runs };
        let counted = vec![distinct_pairs(&mut pairs, 1, &memory).expect("a pair partition")];
        owner.count_distinct(&counted, 0).expect("the pairs count into the groups");

        let output = owner.finish(10, &memory).expect("a mixed radix owner");
        let mut rows = Vec::new();
        for chunk in output.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row: &Vec<Value>| format!("{:?}", row[0]));
        assert_eq!(
            rows,
            [
                vec![
                    Value::Integer(3),
                    Value::HugeInt(5),
                    Value::BigInt(3),
                    Value::Double(5.0),
                    Value::BigInt(2),
                ],
                vec![
                    Value::Integer(4),
                    Value::HugeInt(7),
                    Value::BigInt(1),
                    Value::Double(8.0),
                    Value::BigInt(1),
                ],
                vec![
                    Value::Null,
                    Value::HugeInt(5),
                    Value::BigInt(1),
                    Value::Double(2.0),
                    Value::BigInt(1),
                ],
            ]
        );
        assert_eq!(size_of::<Record>(), 16);
    }

    #[test]
    fn a_pair_comes_back_in_the_split_that_owns_its_group() {
        // What lets the second pass write straight into the group tables the first pass built. The
        // owner a group's numeric records went to is the top bits of its hash, and the split its
        // pairs come back in is picked by the same bits, so the two are the same number.
        for group in [-9_i32, 0, 1, 7, 1_000, i32::MAX] {
            for valid in [true, false] {
                let hash = group_hash(group, valid);
                assert_eq!(crate::pairs::split_of(hash, 16), (hash >> shift()) as usize);
            }
        }
    }
}
