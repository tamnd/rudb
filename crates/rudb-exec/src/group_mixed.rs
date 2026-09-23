//! Direct radix ownership for a mixed numeric and distinct grouped aggregate.
//!
//! The query this is for asks for both kinds of answer about the same groups, a `SUM` and a `COUNT`
//! and an `AVG` alongside a `COUNT(DISTINCT)`, and the two kinds want the rows partitioned on
//! different things. The numeric side wants them on the group, because a group's running total has
//! to be in one place. The distinct side wants them on the pair of the group and the value being
//! counted, because deduplicating is the expensive half and a lopsided grouping column otherwise
//! hands the whole of the biggest group to one thread.
//!
//! So the rows go both ways. Each instance folds the numeric side into a table of its own and
//! buffers a pair record partitioned by the pair hash, and at the end the pairs are deduplicated by
//! [`crate::pairs`] and the survivors are counted into the group tables the numeric side already
//! built. A surviving pair carries its group hash, and the split it comes back in is picked by the
//! top bits of that hash, which is the same arithmetic that picked the group's owner, so the split
//! and the owner are the same number and no group has to be looked for anywhere else.
//!
//! The numeric side is two phases and not one. An instance folds into its own table for as long as
//! that table stays small, and only once it holds more groups than [`LOCAL_GROUPS`] does the
//! instance go back to writing a record per row and partitioning them by the group hash. A local
//! table is worth having exactly when there are many more rows than groups, which is when folding
//! collapses a run of rows into one state before anything is shared, and it stops being worth having
//! when the table no longer sits in cache. Both phases end in the same owners and the fold is
//! associative, so an instance that changes its mind halfway leaves half its rows in each and the
//! answer does not notice.
//!
//! What this replaces is a set of every distinct pair held inside each owner. That set was probed
//! once a row and rehashed as it grew, and on the million row ClickBench file the two functions it
//! amounted to were thirty nine percent of the query.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, Value, stage};
use rudb_kernels::Accumulator;
use rudb_pipeline::Lease;
use rudb_vector::{Chunk, Vector};

use crate::pairs::{
    self, Counted, Held, PARTITIONS, Run, distinct_pairs, in_parallel, scatter_seeded,
};
use crate::rows;
use crate::signed::SignedBlock;

const EMPTY: u32 = u32::MAX;
const FLUSH_ROWS: usize = 32_768;

/// How many groups an instance folds into a table of its own before it starts partitioning instead.
///
/// A [`State`] is sixty four bytes and a bucket is four, so a table at this size is about a third of
/// a megabyte and probes out of the level two cache of the core running it. Past that the probe
/// starts missing, and a table that misses is no cheaper than the owner's table it was put in front
/// of while still costing a merge at the end.
const LOCAL_GROUPS: usize = 4_096;

#[derive(Debug)]
pub(crate) struct Exchange {
    owners: Vec<Mutex<Table>>,
    /// The pairs behind the distinct count, partitioned on the pair and not on the group.
    pairs: Vec<Mutex<Held>>,
    next_start: AtomicUsize,
    held: Mutex<Vec<Reservation>>,
}

/// What identifies a group, kept apart from what is accumulated for it.
///
/// A probe compares this and reads nothing else, so a table of a few thousand groups walks twelve
/// bytes a slot rather than the sixty four a [`State`] takes, and the whole of what a probe touches
/// stays in cache for several times as many groups as it otherwise would.
#[derive(Debug, Clone, Copy)]
struct Key {
    group: i32,
    /// Carried rather than recomputed, because the scatter that made the record already has it and
    /// growing the table would otherwise hash every group it holds again.
    hash: u32,
    valid: bool,
}

impl Key {
    #[inline]
    fn same(self, other: Self) -> bool {
        self.group == other.group && self.valid == other.valid
    }
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

    fn key(self) -> Key {
        Key { group: self.group, hash: self.group_hash, valid: self.has(Self::GROUP) }
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
    /// This instance's own group table, which every numeric row folds into until there are too many
    /// groups for it to be worth keeping.
    table: Table,
    /// Set once [`Self::table`] has outgrown [`LOCAL_GROUPS`], after which rows are partitioned and
    /// folded by the owners as they always were.
    spread: bool,
    partitions: Vec<Partition>,
    pairs: Vec<Run>,
    memory: Reservation,
    /// The group key, the SUM argument, the AVG argument and the distinct argument of one chunk,
    /// read as blocks rather than a row at a time. See [`SignedBlock`].
    blocks: [SignedBlock; 4],
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            buffered: 0,
            table: Table::new(memory),
            spread: false,
            partitions: (0..PARTITIONS).map(|_| Partition::default()).collect(),
            pairs: (0..PARTITIONS).map(|_| Run::default()).collect(),
            memory: memory.reservation(),
            blocks: Default::default(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
    }

    fn footprint(&self) -> usize {
        self.partitions.iter().map(Partition::footprint).sum::<usize>()
            + self.pairs.iter().map(Run::footprint).sum::<usize>()
    }

    /// Takes one row's numeric part, either into this instance's table or into a partition.
    ///
    /// Always inlined, for the reason [`Table::slot`] gives.
    #[inline(always)]
    fn numeric(&mut self, row: Record, shift: u32) -> Result<()> {
        if self.spread {
            self.partitions[(row.group_hash >> shift) as usize].rows.push(row);
            Ok(())
        } else {
            self.table.add_row(row)
        }
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
        let exchange = slot.get_or_init(|| Self {
            owners: (0..PARTITIONS).map(|_| Mutex::new(Table::new(memory))).collect(),
            pairs: (0..PARTITIONS).map(|_| Mutex::new(Held::default())).collect(),
            next_start: AtomicUsize::new(0),
            held: Mutex::new(Vec::new()),
        });
        let timing = stage::Timing::start(Stage::Scatter);
        let before = local.footprint();
        let shift = pairs::shift();
        // The buffers come out of the instance for the length of the loop, because filling them
        // borrows it and folding a row into it borrows it again.
        let mut blocks = std::mem::take(&mut local.blocks);
        let outcome = Self::scatter_blocks(&mut blocks, inputs, rows, shift, local);
        local.blocks = blocks;
        outcome?;
        if !local.spread && local.table.len() > LOCAL_GROUPS {
            local.spread = true;
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

    /// The four columns of one chunk read as blocks and then folded a row at a time.
    ///
    /// Split out of [`Self::buffer`] only so that the buffers can be lent out while the instance is
    /// borrowed for the fold. See [`SignedBlock`] for why they are worth lending.
    ///
    /// The two loops are the same fold twice, once for a chunk with no null in any of the four
    /// columns and once for a chunk with one somewhere. The first is what ClickBench 9 runs and it
    /// reads four flat slices and asks nothing else. The second asks each column about each row, as
    /// it always did, but only for the columns that actually hold a null.
    fn scatter_blocks(
        blocks: &mut [SignedBlock; 4],
        inputs: [&Vector; 4],
        rows: usize,
        shift: u32,
        local: &mut Local,
    ) -> Result<()> {
        let [group, sum, mean, user] = inputs;
        for (held, column) in blocks.iter_mut().zip(inputs) {
            held.read(rows, column)?;
        }
        let [held_group, held_sum, held_mean, held_user] = &*blocks;
        let (null_group, null_sum, null_mean, null_user) =
            (held_group.nulled(), held_sum.nulled(), held_mean.nulled(), held_user.nulled());
        let (held_group, held_sum, held_mean, held_user) = (
            held_group.cut(rows)?,
            held_sum.cut(rows)?,
            held_mean.cut(rows)?,
            held_user.cut(rows)?,
        );
        if !(null_group || null_sum || null_mean || null_user) {
            for row in 0..rows {
                let key = held_group[row] as i32;
                let seed = pairs::group_seed(key, true);
                local.numeric(
                    Record {
                        group: key,
                        group_hash: pairs::folded(seed),
                        sum: held_sum[row] as i16,
                        mean: held_mean[row] as i16,
                        valid: Record::GROUP | Record::SUM | Record::MEAN,
                    },
                    shift,
                )?;
                scatter_seeded(&mut local.pairs, shift, seed, key, true, held_user[row]);
            }
            return Ok(());
        }
        for row in 0..rows {
            let mut valid = 0_u8;
            let key = if null_group && group.is_null_at(row) {
                0
            } else {
                valid |= Record::GROUP;
                i32::try_from(held_group[row])
                    .map_err(|_| Error::internal("an INTEGER group is out of range"))?
            };
            let sum = if null_sum && sum.is_null_at(row) {
                0
            } else {
                valid |= Record::SUM;
                i16::try_from(held_sum[row])
                    .map_err(|_| Error::internal("a SMALLINT sum value is out of range"))?
            };
            let mean = if null_mean && mean.is_null_at(row) {
                0
            } else {
                valid |= Record::MEAN;
                i16::try_from(held_mean[row])
                    .map_err(|_| Error::internal("a SMALLINT average value is out of range"))?
            };
            let held = valid & Record::GROUP != 0;
            let seed = pairs::group_seed(key, held);
            local.numeric(
                Record { group: key, group_hash: pairs::folded(seed), sum, mean, valid },
                shift,
            )?;
            // A null value counts towards nothing, so it never becomes a pair. The row still counts
            // towards the numeric aggregates above, which is why this is the only part of it that is
            // skipped.
            if !(null_user && user.is_null_at(row)) {
                scatter_seeded(&mut local.pairs, shift, seed, key, held, held_user[row]);
            }
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

    /// Folds one instance's own table into the owners, each group into the owner its hash picks.
    ///
    /// The groups are bucketed by owner before any lock is taken, so an owner is locked once for
    /// however many of this instance's groups belong to it rather than once a group. An instance
    /// that saw a thousand groups would otherwise take and drop a thousand locks to hand over a
    /// thousand states.
    fn absorb(&self, table: &mut Table) -> Result<()> {
        if table.is_empty() {
            return Ok(());
        }
        let timing = stage::Timing::start(Stage::Fold);
        let shift = pairs::shift();
        let mut by_owner: Vec<Vec<usize>> = (0..PARTITIONS).map(|_| Vec::new()).collect();
        for (slot, key) in table.keys.iter().enumerate() {
            by_owner[(key.hash >> shift) as usize].push(slot);
        }
        for (at, slots) in by_owner.into_iter().enumerate() {
            if slots.is_empty() {
                continue;
            }
            let mut owner = self.owners[at].lock().map_err(poisoned)?;
            for slot in slots {
                owner.fold(table.keys[slot], &table.states[slot])?;
            }
        }
        table.release();
        timing.stop(0);
        Ok(())
    }

    /// Hands one instance's work over, the numeric states by fold and the pairs by move.
    ///
    /// The pairs are not flushed along the way the numeric records are. There is nothing to fold
    /// them into until every instance has finished, so flushing them early would only copy them into
    /// a shared vector under a lock, where handing the run over at the end is a move.
    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        self.flush(&mut local)?;
        self.absorb(&mut local.table)?;
        for (at, run) in local.pairs.iter_mut().enumerate() {
            if !run.is_empty() {
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
    pub(crate) fn finish(
        &self,
        threads: &Lease<'_>,
        bound: usize,
        memory: &Memory,
    ) -> Result<Vec<Chunk>> {
        let input = self
            .pairs
            .iter()
            .map(|partition| partition.lock().map(|held| held.rows()).map_err(poisoned))
            .sum::<Result<usize>>()?;
        let degree = pairs::finish_degree(input, PARTITIONS.min(threads.degree()));
        // How many of the scattered partitions are worth keeping apart, which the scatter itself
        // could not know. See [`pairs::used`]. The splits below are not the same question: a split
        // has to be an owner, because the owner is what already holds the group's numeric state.
        let used = pairs::used(input, degree);
        let counted = in_parallel(
            threads,
            used,
            degree,
            "deduplicated the pairs of radix partition",
            |at| {
                let mut partition = Held::default();
                for from in pairs::merged(at, used) {
                    let mut held = self.pairs[from].lock().map_err(poisoned)?;
                    partition.runs.append(&mut held.runs);
                }
                distinct_pairs(&mut partition, PARTITIONS, memory)
            },
        )?;
        let outputs =
            in_parallel(threads, PARTITIONS, degree, "finished mixed radix partition", |at| {
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

/// What is accumulated for one group, with nothing in it that says which group that is.
#[derive(Debug, Default)]
struct State {
    count: i64,
    sum: i128,
    sum_seen: bool,
    mean: i128,
    mean_count: i64,
    distinct: i64,
}

/// An open addressed table of groups, used both by an instance for itself and by an owner.
///
/// The two uses are the same code because the second phase has to accept whatever the first phase
/// folded, and a state that came from a whole instance and a state that came from one row differ
/// only in what is in them.
#[derive(Debug)]
struct Table {
    buckets: Vec<u32>,
    /// How many groups this table takes before it is grown, which is half the buckets. Held rather
    /// than worked out, because the alternative is a multiply and a compare on every probe.
    limit: usize,
    keys: Vec<Key>,
    states: Vec<State>,
    memory: Reservation,
}

impl Table {
    fn new(memory: &Memory) -> Self {
        Self {
            buckets: Vec::new(),
            limit: 0,
            keys: Vec::new(),
            states: Vec::new(),
            memory: memory.reservation(),
        }
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The slot of one group, opened if this table has not seen it before.
    ///
    /// Always inlined, and not merely offered for inlining, because the compiler kept saying no and
    /// the call it left behind cost more than the probe inside it. Callgrind on ClickBench 9 put
    /// this at forty seven instructions a row, of which the body is seventeen and the other thirty
    /// are a call, a frame and the registers either side of it. It is one probe of one table and it
    /// runs once per input row, so there is nothing in here worth a call.
    ///
    /// Forcing it, along with [`Table::add_row`] and [`Local::numeric`] above, takes the query from
    /// 1036 million instructions to 896 and from 4.030 ms to 3.490 at thirty two threads. The two
    /// numbers agreeing is what says this was instructions and not something else.
    #[inline(always)]
    fn slot(&mut self, key: Key) -> Result<usize> {
        if self.keys.len() >= self.limit {
            self.grow()?;
        }
        let mask = self.buckets.len() - 1;
        let mut at = key.hash as usize & mask;
        loop {
            let found = self.buckets[at];
            if found == EMPTY {
                return self.open(at, key);
            }
            let found = found as usize;
            if self.keys[found].same(key) {
                return Ok(found);
            }
            at = (at + 1) & mask;
        }
    }

    /// Puts a group this table has not seen into the bucket the probe stopped on.
    ///
    /// Out of line from [`Self::slot`] because it runs once a group where the probe runs once a row,
    /// and the reservation it takes has no business being on the path a hit takes.
    #[cold]
    fn open(&mut self, at: usize, key: Key) -> Result<usize> {
        let slot = self.keys.len();
        self.buckets[at] = u32::try_from(slot)
            .map_err(|_| Error::out_of_memory("too many mixed aggregate groups"))?;
        if self.states.len() == self.states.capacity() {
            let old = self.states.capacity();
            let new = old.max(16) * 2;
            self.memory.grow(width((new - old) * (size_of::<State>() + size_of::<Key>())))?;
            self.states.reserve_exact(new - old);
            self.keys.reserve_exact(new - old);
        }
        self.keys.push(key);
        self.states.push(State::default());
        Ok(slot)
    }

    /// One row's numeric part folded into its group.
    ///
    /// Always inlined, for the reason [`Table::slot`] gives.
    #[inline(always)]
    fn add_row(&mut self, row: Record) -> Result<()> {
        let slot = self.slot(row.key())?;
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
        Ok(())
    }

    fn add_all(&mut self, rows: &mut Vec<Record>) -> Result<()> {
        for row in rows.drain(..) {
            self.add_row(row)?;
        }
        Ok(())
    }

    /// Adds one whole state to whatever this table already holds for that group.
    ///
    /// What makes the two phases agree. An instance adds up whatever share of a group it happened to
    /// see and the owner adds the shares, which is the same answer because every part of a state
    /// here is a sum and a sum does not care how it was bracketed.
    fn fold(&mut self, key: Key, add: &State) -> Result<()> {
        let slot = self.slot(key)?;
        let state = &mut self.states[slot];
        state.count = state
            .count
            .checked_add(add.count)
            .ok_or_else(|| Error::out_of_range("a mixed COUNT overflowed BIGINT"))?;
        if add.sum_seen {
            state.sum = state
                .sum
                .checked_add(add.sum)
                .ok_or_else(|| Error::out_of_range("a mixed SUM overflowed HUGEINT"))?;
            state.sum_seen = true;
        }
        if add.mean_count != 0 {
            state.mean = state
                .mean
                .checked_add(add.mean)
                .ok_or_else(|| Error::out_of_range("a mixed AVG total overflowed HUGEINT"))?;
            state.mean_count = state
                .mean_count
                .checked_add(add.mean_count)
                .ok_or_else(|| Error::out_of_range("a mixed AVG count overflowed BIGINT"))?;
        }
        if add.distinct != 0 {
            state.distinct = state
                .distinct
                .checked_add(add.distinct)
                .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
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
                let key = Key { group: pair.group, hash: pair.group_hash, valid: pair.valid };
                let slot = self.slot(key)?;
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

    fn grow(&mut self) -> Result<()> {
        let old = self.buckets.len();
        let new = old.max(32) * 2;
        self.memory.grow(width(new * size_of::<u32>()))?;
        let mut grown = vec![EMPTY; new];
        let mask = new - 1;
        for (slot, key) in self.keys.iter().enumerate() {
            let mut at = key.hash as usize & mask;
            while grown[at] != EMPTY {
                at = (at + 1) & mask;
            }
            grown[at] = slot as u32;
        }
        self.buckets = grown;
        self.limit = new / 2;
        self.memory.shrink(width(old * size_of::<u32>()));
        Ok(())
    }

    /// Gives back everything this table holds, for a local one that has handed its states over.
    fn release(&mut self) {
        self.buckets = Vec::new();
        self.limit = 0;
        self.keys = Vec::new();
        self.states = Vec::new();
        self.memory.release();
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
            let key = self.keys[slot];
            let state = &self.states[slot];
            let group = if key.valid { Value::Integer(key.group) } else { Value::Null };
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
        self.release();
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

    use rudb_common::{Memory, Value};

    use crate::pairs::{Held, PARTITIONS, Run, distinct_pairs, group_hash, scatter, shift};

    use super::{Key, LOCAL_GROUPS, Local, Record, State, Table};

    #[test]
    fn one_owner_combines_numeric_and_distinct_states() {
        // The same five rows go both ways, as numeric records here and as pairs below, which is what
        // the operator does with them. One split and one owner, because an owner in a real query
        // only ever sees the split its own groups came back in.
        let all = Record::GROUP | Record::SUM | Record::MEAN;
        let mut input = vec![
            row(3, 2, 4, all),
            row(3, 3, 6, all),
            row(3, 0, 0, Record::GROUP),
            row(4, 7, 8, all),
            row(0, 5, 2, Record::SUM | Record::MEAN),
        ];
        let memory = Memory::unlimited();
        let mut owner = Table::new(&memory);
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

        assert_eq!(emitted(&mut owner, &memory), expected());
        assert_eq!(size_of::<Record>(), 16);
    }

    /// The same rows folded locally first come out the same as the rows folded straight in.
    ///
    /// The point of the two phases. An instance adds up whatever share of a group it saw before
    /// anything is shared, and the owner adds the shares, so the same five rows split across two
    /// instances have to give what one owner reading all five gives. The distinct side is left out
    /// here because it never goes through a local table at all, and the test above covers it.
    #[test]
    fn folding_locally_and_then_into_an_owner_gives_what_folding_straight_in_gives() {
        let all = Record::GROUP | Record::SUM | Record::MEAN;
        let rows = [
            row(3, 2, 4, all),
            row(3, 3, 6, all),
            row(3, 0, 0, Record::GROUP),
            row(4, 7, 8, all),
            row(0, 5, 2, Record::SUM | Record::MEAN),
        ];
        let memory = Memory::unlimited();
        let mut straight = Table::new(&memory);
        straight.add_all(&mut rows.to_vec()).expect("rows enter one owner");

        // Two instances that between them saw the same five rows, each folding its own share.
        let mut owner = Table::new(&memory);
        for share in rows.chunks(2) {
            let mut instance = Table::new(&memory);
            instance.add_all(&mut share.to_vec()).expect("rows enter one instance");
            for (slot, key) in instance.keys.iter().enumerate() {
                owner.fold(*key, &instance.states[slot]).expect("a state enters its owner");
            }
        }
        assert_eq!(emitted(&mut owner, &memory), emitted(&mut straight, &memory));
    }

    /// An instance stops keeping a table of its own once the table is too big to be worth it.
    ///
    /// Without this a grouping column with a million distinct values would build a million entry
    /// table per instance in front of the owners, which is every cost of the table and none of the
    /// collapsing it is there for.
    #[test]
    fn an_instance_gives_up_its_own_table_once_it_holds_too_many_groups() {
        let memory = Memory::unlimited();
        let mut local = Local::new(&memory);
        let shift = shift();
        for group in 0..i32::try_from(LOCAL_GROUPS).expect("a small bound") {
            local
                .numeric(row(group, 1, 1, Record::GROUP | Record::SUM | Record::MEAN), shift)
                .expect("a row");
        }
        assert_eq!(local.table.len(), LOCAL_GROUPS, "every group so far is its own");
        assert!(!local.spread, "the table is still inside the bound");
        assert!(local.partitions.iter().all(|part| part.rows.is_empty()), "nothing partitioned");

        local.spread = local.table.len() > LOCAL_GROUPS;
        assert!(!local.spread, "the bound is inclusive");
        local.numeric(row(-1, 1, 1, Record::GROUP), shift).expect("one more group");
        local.spread = local.table.len() > LOCAL_GROUPS;
        assert!(local.spread, "one group past the bound is one too many");

        local.numeric(row(-2, 1, 1, Record::GROUP), shift).expect("a partitioned row");
        assert_eq!(local.table.len(), LOCAL_GROUPS + 1, "the table stopped where it was");
        assert_eq!(
            local.partitions.iter().map(|part| part.rows.len()).sum::<usize>(),
            1,
            "and the row after it was partitioned instead"
        );
    }

    #[test]
    fn a_pair_comes_back_in_the_split_that_owns_its_group() {
        // What lets the second pass write straight into the group tables the first pass built. The
        // owner a group's numeric records went to is the top bits of its hash, and the split its
        // pairs come back in is picked by the same bits, so the two are the same number.
        for group in [-9_i32, 0, 1, 7, 1_000, i32::MAX] {
            for valid in [true, false] {
                let hash = group_hash(group, valid);
                assert_eq!(crate::pairs::split_of(hash, PARTITIONS), (hash >> shift()) as usize);
            }
        }
        assert_eq!(size_of::<Key>(), 12);
        assert_eq!(size_of::<State>(), 64);
    }

    fn row(group: i32, sum: i16, mean: i16, valid: u8) -> Record {
        Record {
            group,
            group_hash: group_hash(group, valid & Record::GROUP != 0),
            sum,
            mean,
            valid,
        }
    }

    /// One table's rows, sorted so that the order the groups were opened in does not show.
    fn emitted(table: &mut Table, memory: &Memory) -> Vec<Vec<Value>> {
        let output = table.finish(10, memory).expect("a mixed radix owner");
        let mut rows = Vec::new();
        for chunk in output.chunks {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|column| chunk.value_at(row, column)).collect());
            }
        }
        rows.sort_by_key(|row: &Vec<Value>| format!("{:?}", row[0]));
        rows
    }

    fn expected() -> Vec<Vec<Value>> {
        vec![
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
    }
}
