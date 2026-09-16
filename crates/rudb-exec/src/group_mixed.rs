//! Direct radix ownership for a mixed numeric and distinct grouped aggregate.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Spent, Stage, Value, stage};
use rudb_kernels::Accumulator;
use rudb_vector::{Chunk, Data, Packed, Vector};

use crate::key::{mix, spread};
use crate::rows;

const PARTITIONS: usize = 16;
const EMPTY: u32 = u32::MAX;
const NULL_GROUP: u64 = 0x9e37_79b9_7f4a_7c15;
const FLUSH_ROWS: usize = 32_768;
const PAIR_ODD: u64 = 0x517c_c1b7_2722_0a95;

fn pair_hash(group_hash: u32, user: i64) -> u64 {
    (user as u64).wrapping_mul(PAIR_ODD) ^ (u64::from(group_hash) << 32 | u64::from(group_hash))
}

fn group_hash(group: i32, valid: bool) -> u32 {
    let word = if valid { i64::from(group) as u64 } else { NULL_GROUP };
    let wide = spread(mix(0, word));
    (wide ^ (wide >> 32)) as u32
}

enum SignedReader<'a> {
    Int16(&'a [i16]),
    Int32(&'a [i32]),
    Int64(&'a [i64]),
    Packed(Packed<'a>),
    Other(&'a Vector),
}

impl<'a> SignedReader<'a> {
    fn new(vector: &'a Vector) -> Self {
        match vector.data() {
            Some(Data::Int16(values)) => Self::Int16(values.as_slice()),
            Some(Data::Int32(values)) => Self::Int32(values.as_slice()),
            Some(Data::Int64(values)) => Self::Int64(values.as_slice()),
            _ => match vector.packed_parts() {
                Some(packed) => Self::Packed(packed),
                None => Self::Other(vector),
            },
        }
    }

    fn at(&self, row: usize) -> i128 {
        match self {
            Self::Int16(values) => i128::from(values[row]),
            Self::Int32(values) => i128::from(values[row]),
            Self::Int64(values) => i128::from(values[row]),
            Self::Packed(packed) => {
                let words = packed.words();
                let width = packed.width();
                let bit = (packed.offset() + row) * width as usize;
                let word = bit / u64::BITS as usize;
                let shift = (bit % u64::BITS as usize) as u32;
                let mask = u64::MAX >> (u64::BITS - width);
                let low = words.get(word).copied().unwrap_or(0) >> shift;
                let taken = u64::BITS - shift;
                let code = if taken >= width {
                    low & mask
                } else {
                    let high = words.get(word + 1).copied().unwrap_or(0) << taken;
                    (low | high) & mask
                };
                packed.base() + i128::from(code)
            }
            Self::Other(vector) => {
                vector.signed_at(row).expect("the typed mixed aggregate input is a signed value")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Exchange {
    owners: Vec<Mutex<Owner>>,
    next_start: AtomicUsize,
    held: Mutex<Vec<Reservation>>,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    user: i64,
    group: i32,
    group_hash: u32,
    sum: i16,
    mean: i16,
    valid: u8,
}

impl Record {
    const GROUP: u8 = 1;
    const USER: u8 = 1 << 1;
    const SUM: u8 = 1 << 2;
    const MEAN: u8 = 1 << 3;

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
    memory: Reservation,
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            used: false,
            buffered: 0,
            partitions: (0..PARTITIONS).map(|_| Partition::default()).collect(),
            memory: memory.reservation(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.used
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
            next_start: AtomicUsize::new(0),
            held: Mutex::new(Vec::new()),
        });
        let timing = stage::Timing::start(Stage::Scatter);
        let before = local.partitions.iter().map(Partition::footprint).sum::<usize>();
        let shift = u32::BITS - PARTITIONS.ilog2();
        let all_valid = inputs.iter().all(|column| !column.validity().has_nulls(rows));
        if all_valid {
            let group = SignedReader::new(group);
            let sum = SignedReader::new(sum);
            let mean = SignedReader::new(mean);
            let user = SignedReader::new(user);
            for row in 0..rows {
                let group = group.at(row) as i32;
                let group_hash = group_hash(group, true);
                local.partitions[(group_hash >> shift) as usize].rows.push(Record {
                    user: user.at(row) as i64,
                    group,
                    group_hash,
                    sum: sum.at(row) as i16,
                    mean: mean.at(row) as i16,
                    valid: Record::GROUP | Record::USER | Record::SUM | Record::MEAN,
                });
            }
        } else {
            for row in 0..rows {
                let mut valid = 0_u8;
                let group = if group.is_null_at(row) {
                    0
                } else {
                    valid |= Record::GROUP;
                    i32::try_from(group.signed_at(row).ok_or_else(|| {
                        Error::internal("an INTEGER group has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("an INTEGER group is out of range"))?
                };
                let user = if user.is_null_at(row) {
                    0
                } else {
                    valid |= Record::USER;
                    i64::try_from(user.signed_at(row).ok_or_else(|| {
                        Error::internal("a distinct BIGINT value has no signed representation")
                    })?)
                    .map_err(|_| Error::internal("a distinct BIGINT value is out of range"))?
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
                let group_hash = group_hash(group, valid & Record::GROUP != 0);
                local.partitions[(group_hash >> shift) as usize].rows.push(Record {
                    user,
                    group,
                    group_hash,
                    sum,
                    mean,
                    valid,
                });
            }
        }
        let after = local.partitions.iter().map(Partition::footprint).sum::<usize>();
        local.memory.grow(width(after.saturating_sub(before)))?;
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

    pub(crate) fn combine(&self, mut local: Local) -> Result<()> {
        self.flush(&mut local)
    }

    pub(crate) fn finish(&self, bound: usize, memory: &Memory) -> Result<Vec<Chunk>> {
        let input = self
            .owners
            .iter()
            .map(|owner| owner.lock().map(|owner| owner.pairs.len).map_err(poisoned))
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
                let spent =
                    handle.join().map_err(|_| Error::internal("a mixed radix worker panicked"))?;
                theirs.add(spent);
            }
            stage::gained(theirs);
            let mut outputs = Vec::with_capacity(PARTITIONS);
            for (at, slot) in slots.iter().enumerate() {
                outputs.push(slot.lock().map_err(poisoned)?.take().unwrap_or_else(|| {
                    Err(Error::internal(format!("nothing finished mixed radix partition {at}")))
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
            let Some(owner) = self.owners.get(at) else { return };
            let done =
                owner.lock().map_err(poisoned).and_then(|mut owner| owner.finish(bound, memory));
            if let Ok(mut slot) = slots[at].lock() {
                *slot = Some(done);
            }
        }
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

#[derive(Debug, Default)]
struct PairSet {
    controls: Vec<u16>,
    users: Vec<i64>,
    groups: Vec<i32>,
    len: usize,
}

impl PairSet {
    fn footprint(capacity: usize) -> usize {
        capacity * (size_of::<u16>() + size_of::<i64>() + size_of::<i32>())
    }

    fn insert(&mut self, row: Record, memory: &mut Reservation) -> Result<bool> {
        if self.controls.is_empty() || (self.len + 1) * 8 > self.controls.len() * 7 {
            self.grow(memory)?;
        }
        Ok(self.insert_unchecked(row))
    }

    fn insert_unchecked(&mut self, row: Record) -> bool {
        let hash = pair_hash(row.group_hash, row.user);
        let valid = row.has(Record::GROUP);
        let tag = 0x8000 | ((valid as u16) << 14) | ((hash >> 50) as u16 & 0x3fff);
        let mask = self.controls.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let held = self.controls[at];
            if held == 0 {
                self.controls[at] = tag;
                self.users[at] = row.user;
                self.groups[at] = row.group;
                self.len += 1;
                return true;
            }
            if held == tag && self.users[at] == row.user && self.groups[at] == row.group {
                return false;
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self, memory: &mut Reservation) -> Result<()> {
        let old = self.controls.len();
        let new = old.max(32) * 2;
        let old_bytes = Self::footprint(old);
        let new_bytes = Self::footprint(new);
        memory.grow(width(new_bytes))?;
        let mut grown =
            Self { controls: vec![0; new], users: vec![0; new], groups: vec![0; new], len: 0 };
        for at in 0..old {
            if self.controls[at] == 0 {
                continue;
            }
            let valid = self.controls[at] & (1 << 14) != 0;
            let group_word = if valid { i64::from(self.groups[at]) as u64 } else { NULL_GROUP };
            let group_wide = spread(mix(0, group_word));
            let group_hash = (group_wide ^ (group_wide >> 32)) as u32;
            let valid = Record::USER | if valid { Record::GROUP } else { 0 };
            grown.insert_unchecked(Record {
                user: self.users[at],
                group: self.groups[at],
                group_hash,
                sum: 0,
                mean: 0,
                valid,
            });
        }
        *self = grown;
        memory.shrink(width(old_bytes));
        Ok(())
    }
}

#[derive(Debug)]
struct Owner {
    buckets: Vec<u32>,
    states: Vec<State>,
    pairs: PairSet,
    memory: Reservation,
}

impl Owner {
    fn new(memory: &Memory) -> Self {
        Self {
            buckets: Vec::new(),
            states: Vec::new(),
            pairs: PairSet::default(),
            memory: memory.reservation(),
        }
    }

    fn add_all(&mut self, rows: &mut Vec<Record>) -> Result<()> {
        for row in rows.drain(..) {
            let slot = self.group(row)?;
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
            if row.has(Record::USER) && self.pairs.insert(row, &mut self.memory)? {
                state.distinct = state
                    .distinct
                    .checked_add(1)
                    .ok_or_else(|| Error::out_of_range("COUNT(DISTINCT BIGINT) overflowed"))?;
            }
        }
        Ok(())
    }

    fn group(&mut self, row: Record) -> Result<usize> {
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
        let mut at = row.group_hash as usize & mask;
        loop {
            let slot = self.buckets[at];
            if slot == EMPTY {
                let slot = self.states.len();
                self.buckets[at] = u32::try_from(slot)
                    .map_err(|_| Error::out_of_memory("too many mixed aggregate groups"))?;
                self.states.push(State {
                    group: row.group,
                    group_valid: row.has(Record::GROUP),
                    ..State::default()
                });
                return Ok(slot);
            }
            let slot = slot as usize;
            let held = &self.states[slot];
            if held.group == row.group && held.group_valid == row.has(Record::GROUP) {
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
            let word = if state.group_valid { i64::from(state.group) as u64 } else { NULL_GROUP };
            let wide = spread(mix(0, word));
            let hash = (wide ^ (wide >> 32)) as u32;
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
        self.pairs = PairSet::default();
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
        let row =
            |group, user, sum, mean, valid| Record { user, group, group_hash: 7, sum, mean, valid };
        let all = Record::GROUP | Record::USER | Record::SUM | Record::MEAN;
        let mut input = vec![
            row(3, 10, 2, 4, all),
            row(3, 10, 3, 6, all),
            row(3, 11, 0, 0, Record::GROUP | Record::USER),
            row(4, 10, 7, 8, all),
            row(0, 10, 5, 2, Record::USER | Record::SUM | Record::MEAN),
        ];
        let memory = Memory::unlimited();
        let mut owner = Owner::new(&memory);
        owner.add_all(&mut input).expect("rows enter one owner");
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
        assert_eq!(size_of::<Record>(), 24);
    }
}
