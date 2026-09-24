//! Counts grouped by one integer key whose range the planner knows, kept in arrays the key indexes.
//!
//! TPC-H q13 groups the rows of a left join by `c_custkey` and counts `o_orderkey` in each group.
//! The key is an integer between one and the number of customers, and `rudb_opt`'s `dense` pass
//! already says so. The general table used that range only to find a group's slot faster. Every
//! group still took a hashed bucket, a copy of its key, a fresh accumulator and a probe on the way
//! in, and the unmatched customers of the join came out of an instance of their own, which made the
//! aggregate partition and hash every group of the probe's table a second time. At one scale factor that was about two hundred
//! and twenty instructions a row, where the question the query asks of a row is one add.
//!
//! So a `COUNT(*)` or `COUNT(x)` grouped this way is counted straight into arrays as long as the
//! range. A row costs a subtract, a compare and an add per call. Two instances combine by adding
//! their arrays, and the answer is the places whose row count is not zero, in key order.
//!
//! The range is the one the values are inside of, so nothing should ever land outside it. A value
//! that does anyway is counted in a small map beside the arrays, which keeps a wrong bound from
//! ever turning into a wrong answer, the same promise the direct index in the general table makes.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, Reservation, Result, Stage, stage};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::signed::SignedBlock;

/// What one call counts in a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    /// Every row, which is `COUNT(*)`.
    Rows,
    /// The rows where the call's one argument is not null, which is `COUNT(x)`.
    Valid,
}

/// The shared half, which the instances add their arrays into as they finish.
#[derive(Debug)]
pub(crate) struct Exchange {
    key: LogicalType,
    low: i64,
    width: usize,
    calls: Vec<Counted>,
    total: Mutex<Option<Tallies>>,
    held: Mutex<Vec<Reservation>>,
}

/// One instance's half.
#[derive(Debug)]
pub(crate) struct Local {
    tallies: Option<Tallies>,
    block: SignedBlock,
    places: Vec<u32>,
    memory: Reservation,
}

/// The counts, one array per call and one for the rows, each `width + 2` long.
///
/// The place past the range is the null key's and the one past that takes the rows whose value
/// the range does not cover, which are counted again in `outside`. Giving those rows a place the
/// answer never reads keeps the loops over the places free of a branch.
#[derive(Debug)]
struct Tallies {
    rows: Vec<i64>,
    valid: Vec<Vec<i64>>,
    outside: HashMap<i64, Vec<i64>>,
}

impl Tallies {
    fn new(width: usize, calls: &[Counted]) -> Self {
        let valid = calls.iter().filter(|&&call| call == Counted::Valid).count();
        Self {
            rows: vec![0; width + 2],
            valid: (0..valid).map(|_| vec![0; width + 2]).collect(),
            outside: HashMap::new(),
        }
    }

    fn footprint(&self) -> usize {
        (self.rows.len() * (1 + self.valid.len())) * size_of::<i64>()
    }

    fn add(&mut self, other: Self) {
        for (into, from) in self.rows.iter_mut().zip(&other.rows) {
            *into += from;
        }
        for (into, from) in self.valid.iter_mut().zip(&other.valid) {
            for (into, from) in into.iter_mut().zip(from) {
                *into += from;
            }
        }
        for (key, counts) in other.outside {
            let held = self.outside.entry(key).or_insert_with(|| vec![0; counts.len()]);
            for (into, from) in held.iter_mut().zip(&counts) {
                *into += from;
            }
        }
    }
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            tallies: None,
            block: SignedBlock::default(),
            places: Vec::new(),
            memory: memory.reservation(),
        }
    }

    pub(crate) fn used(&self) -> bool {
        self.tallies.is_some()
    }
}

impl Exchange {
    pub(crate) fn new(key: LogicalType, low: i64, width: usize, calls: Vec<Counted>) -> Self {
        Self { key, low, width, calls, total: Mutex::new(None), held: Mutex::new(Vec::new()) }
    }

    /// Counts one chunk into this instance's arrays.
    ///
    /// `arguments` holds the one argument of each call, and nothing for a `COUNT(*)`, and each is
    /// `rows` long.
    pub(crate) fn count(
        &self,
        key: &Vector,
        arguments: &[Option<&Vector>],
        rows: usize,
        local: &mut Local,
    ) -> Result<()> {
        let timing = stage::Timing::start(Stage::Fold);
        if local.tallies.is_none() {
            let tallies = Tallies::new(self.width, &self.calls);
            local.memory.grow(u64::try_from(tallies.footprint()).unwrap_or(u64::MAX))?;
            local.tallies = Some(tallies);
        }
        let Local { tallies, block, places, .. } = local;
        let tallies = tallies.as_mut().expect("made above");
        block.read(rows, key)?;
        let values = block.cut(rows)?;
        let (width, low) = (self.width, self.low);
        let outside = width + 1;
        places.clear();
        places.extend(values.iter().map(|&value| {
            let place = value.wrapping_sub(low) as u64;
            if place < width as u64 { place as u32 } else { outside as u32 }
        }));
        if block.nulled() {
            for (row, place) in places.iter_mut().enumerate() {
                if key.is_null_at(row) {
                    *place = width as u32;
                }
            }
        }
        for &place in places.iter() {
            tallies.rows[place as usize] += 1;
        }
        let mut valid = tallies.valid.iter_mut();
        for (call, argument) in self.calls.iter().zip(arguments) {
            if *call != Counted::Valid {
                continue;
            }
            let into = valid.next().expect("one array per COUNT(x)");
            let argument =
                argument.ok_or_else(|| Error::internal("a COUNT(x) with no argument"))?;
            if argument.none_null() {
                for &place in places.iter() {
                    into[place as usize] += 1;
                }
            } else {
                let validity = argument.validity();
                for (row, &place) in places.iter().enumerate() {
                    into[place as usize] += i64::from(validity.is_valid(row));
                }
            }
        }
        // The rows the range did not cover, which should be none, counted again by their value.
        if tallies.rows[outside] != 0 {
            tallies.rows[outside] = 0;
            for (row, &place) in places.iter().enumerate() {
                if place as usize != outside {
                    continue;
                }
                let mut counts = Vec::with_capacity(self.calls.len());
                for (call, argument) in self.calls.iter().zip(arguments) {
                    counts.push(match (call, argument) {
                        (Counted::Valid, Some(argument)) => {
                            i64::from(argument.validity().is_valid(row))
                        }
                        _ => 1,
                    });
                }
                let held =
                    tallies.outside.entry(values[row]).or_insert_with(|| vec![0; counts.len()]);
                for (into, from) in held.iter_mut().zip(counts) {
                    *into += from;
                }
            }
        }
        timing.stop(0);
        Ok(())
    }

    /// Adds one instance's arrays into the shared ones, or hands them over whole to the first.
    pub(crate) fn combine(&self, local: Local) -> Result<()> {
        let Local { tallies, memory, .. } = local;
        let Some(tallies) = tallies else { return Ok(()) };
        let mut total = self.total.lock().map_err(poisoned)?;
        match total.as_mut() {
            Some(held) => held.add(tallies),
            None => *total = Some(tallies),
        }
        drop(total);
        self.held.lock().map_err(poisoned)?.push(memory);
        Ok(())
    }

    /// The groups in key order, then the null key's, then any the range did not cover.
    pub(crate) fn finish(&self, memory: &Memory) -> Result<Vec<Chunk>> {
        let timing = stage::Timing::start(Stage::Emit);
        let Some(tallies) = self.total.lock().map_err(poisoned)?.take() else {
            return Ok(Vec::new());
        };
        let mut charge = memory.reservation();
        let mut chunks = Vec::new();
        let mut out = Out::new(self.calls.len());
        let valid_of = |call: usize| {
            self.calls[..call].iter().filter(|&&counted| counted == Counted::Valid).count()
        };
        let lanes: Vec<Option<usize>> = (0..self.calls.len())
            .map(|call| (self.calls[call] == Counted::Valid).then(|| valid_of(call)))
            .collect();
        for place in 0..=self.width {
            let rows = tallies.rows[place];
            if rows == 0 {
                continue;
            }
            let key = (place < self.width).then(|| self.low + place as i64);
            out.keys.push(key);
            for (call, lane) in lanes.iter().enumerate() {
                out.counts[call].push(match lane {
                    Some(lane) => tallies.valid[*lane][place],
                    None => rows,
                });
            }
            if out.keys.len() == VECTOR_SIZE {
                chunks.push(out.chunk(&self.key, &mut charge)?);
            }
        }
        let mut outside: Vec<(i64, Vec<i64>)> = tallies.outside.into_iter().collect();
        outside.sort_unstable_by_key(|(key, _)| *key);
        for (key, counts) in outside {
            out.keys.push(Some(key));
            for (call, count) in counts.into_iter().enumerate() {
                out.counts[call].push(count);
            }
            if out.keys.len() == VECTOR_SIZE {
                chunks.push(out.chunk(&self.key, &mut charge)?);
            }
        }
        if !out.keys.is_empty() {
            chunks.push(out.chunk(&self.key, &mut charge)?);
        }
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        held.push(charge);
        timing.stop(0);
        Ok(chunks)
    }
}

/// The groups of the chunk being built.
struct Out {
    keys: Vec<Option<i64>>,
    counts: Vec<Vec<i64>>,
}

impl Out {
    fn new(calls: usize) -> Self {
        Self { keys: Vec::with_capacity(VECTOR_SIZE), counts: vec![Vec::new(); calls] }
    }

    fn chunk(&mut self, ty: &LogicalType, charge: &mut Reservation) -> Result<Chunk> {
        let rows = self.keys.len();
        let values = self.keys.iter().map(|key| key.unwrap_or(0));
        let data = match ty {
            LogicalType::TinyInt => {
                Data::Int8(values.map(|value| value as i8).collect::<Vec<_>>().into())
            }
            LogicalType::SmallInt => {
                Data::Int16(values.map(|value| value as i16).collect::<Vec<_>>().into())
            }
            LogicalType::Integer => {
                Data::Int32(values.map(|value| value as i32).collect::<Vec<_>>().into())
            }
            LogicalType::BigInt => Data::Int64(values.collect::<Vec<_>>().into()),
            _ => return Err(Error::internal(format!("{ty} is not a ranged group key"))),
        };
        let mut key = Vector::flat(ty.clone(), data)?;
        if self.keys.iter().any(Option::is_none) {
            let keys = &self.keys;
            key = key.with_validity(Validity::from_iter(rows, |row| keys[row].is_some()));
        }
        let mut columns = Vec::with_capacity(1 + self.counts.len());
        columns.push(key);
        for counts in &mut self.counts {
            columns.push(Vector::flat(
                LogicalType::BigInt,
                Data::Int64(std::mem::take(counts).into()),
            )?);
        }
        self.keys.clear();
        charge.grow(
            u64::try_from(rows * (1 + columns.len()) * size_of::<i64>()).unwrap_or(u64::MAX),
        )?;
        Chunk::with_rows(columns, rows)
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::internal("a ranged count lock was poisoned")
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Memory, Value};
    use rudb_vector::Vector;

    use super::{Counted, Exchange, Local};

    /// Every row of the answer, as values.
    fn answer(exchange: &Exchange, memory: &Memory) -> Vec<Vec<Value>> {
        let chunks = exchange.finish(memory).expect("finishes");
        let mut out = Vec::new();
        for chunk in chunks {
            for row in 0..chunk.len() {
                out.push(
                    (0..chunk.width())
                        .map(|at| chunk.column(at).expect("a column").value_at(row))
                        .collect(),
                );
            }
        }
        out
    }

    /// A value the range does not cover is still counted, in its own group after the ones the
    /// arrays hold, and two instances add up to what one would have counted.
    #[test]
    fn a_value_outside_the_range_is_counted_beside_it_and_instances_add_up() {
        let memory = Memory::unlimited();
        let exchange =
            Exchange::new(LogicalType::Integer, 10, 5, vec![Counted::Rows, Counted::Valid]);
        let key = Vector::from_values(
            LogicalType::Integer,
            &[
                Value::Integer(10),
                Value::Integer(14),
                Value::Null,
                Value::Integer(99),
                Value::Integer(10),
            ],
        )
        .expect("keys");
        let argument = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(1), Value::Null, Value::BigInt(3), Value::Null, Value::BigInt(5)],
        )
        .expect("arguments");
        for _ in 0..2 {
            let mut local = Local::new(&memory);
            exchange.count(&key, &[None, Some(&argument)], 5, &mut local).expect("counts");
            exchange.combine(local).expect("combines");
        }
        let int = Value::Integer;
        let big = Value::BigInt;
        assert_eq!(
            answer(&exchange, &memory),
            vec![
                vec![int(10), big(4), big(4)],
                vec![int(14), big(2), big(0)],
                vec![Value::Null, big(2), big(2)],
                vec![int(99), big(2), big(0)],
            ]
        );
    }
}
