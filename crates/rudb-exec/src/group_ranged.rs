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
//! A `SUM(x)` of a signed integer or a decimal stored in 64 bits goes the same way, since TPC-H q15
//! groups by `l_suppkey`, q11 by `ps_partkey` and q10 by `o_custkey`, each a dense range and each a
//! sum. With few rows to a group every instance of the general table made nearly every group of its
//! own and the merge then made them all again, so eight threads cost close to twice the work of
//! one. Here a row is the same add into a 128 bit total, and a count of the values that were not
//! null beside it, which tells a group whose values were all null to answer null. A sum of 64 bit
//! values cannot overflow that total, and a sum of 128 bit ones, which is what q11's
//! `DECIMAL(34,2)` product is, checks each add the way the general sum does.
//!
//! What a call counts is the rows of a group less its nulls, and not its values that are not null.
//! The two are the same number, but the first is a pass over the places only when a chunk has a
//! null in it, and on q13 none does. Counting the values that were not null was a second add at
//! every row's place, into an array of a million bytes and more that the first add had already
//! missed the cache on, and those two passes were a third of the query.
//!
//! The range is the one the values are inside of, so nothing should ever land outside it. A value
//! that does anyway is counted in a small map beside the arrays, which keeps a wrong bound from
//! ever turning into a wrong answer, the same promise the direct index in the general table makes.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Mutex;

use rudb_common::{Error, LogicalType, Memory, PhysicalType, Reservation, Result, Stage, stage};
use rudb_vector::{Chunk, Data, VECTOR_SIZE, Validity, Vector};

use crate::signed::SignedBlock;

/// What one call counts in a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Counted {
    /// Every row, which is `COUNT(*)`.
    Rows,
    /// The rows where the call's one argument is not null, which is `COUNT(x)`.
    Valid,
    /// The call's one argument added up, which is `SUM(x)`, answered as the 128 bit type given.
    Sum(LogicalType),
}

impl Counted {
    /// Whether the call keeps a count of the values that were not null.
    fn counts_valid(&self) -> bool {
        !matches!(self, Self::Rows)
    }
}

/// Where one call's state lives in the tallies.
#[derive(Debug, Clone, Copy)]
struct Lane {
    nulls: Option<usize>,
    sum: Option<usize>,
}

/// The shared half, which the instances add their arrays into as they finish.
#[derive(Debug)]
pub(crate) struct Exchange {
    key: LogicalType,
    low: i64,
    width: usize,
    calls: Vec<Counted>,
    lanes: Vec<Lane>,
    total: Mutex<Option<Tallies>>,
    held: Mutex<Vec<Reservation>>,
}

/// One instance's half.
#[derive(Debug)]
pub(crate) struct Local {
    tallies: Option<Tallies>,
    block: SignedBlock,
    argument: SignedBlock,
    places: Vec<u32>,
    memory: Reservation,
}

/// The counts, one array of cells for the rows and the sums and one per call that counts values,
/// each over `width + 2` places.
///
/// A place's row count and its sums sit together, `stride` cells to a place, the count first and
/// then each sum as its low and its high half. A chunk's rows land at places all over the range, so
/// every add is a miss, and with the rows in one array and each sum in another a row of q10 missed
/// twice, once for its count and once for its sum. The stride is a power of two, so a place never
/// straddles two lines and the second add finds the line the first one brought in.
///
/// A call that counts values keeps the nulls it saw rather than the values, and its array is empty
/// until the first null comes, so a column with none costs no array and no pass at all.
///
/// The place past the range is the null key's and the one past that takes the rows whose value
/// the range does not cover, which are counted again in `outside`. Giving those rows a place the
/// answer never reads keeps the loops over the places free of a branch.
#[derive(Debug)]
struct Tallies {
    cells: Vec<i64>,
    stride: usize,
    sums: usize,
    nulls: Vec<Vec<i64>>,
    outside: HashMap<i64, Vec<(i64, i128)>>,
}

impl Tallies {
    fn new(width: usize, calls: &[Counted]) -> Self {
        let valid = calls.iter().filter(|call| call.counts_valid()).count();
        let sums = calls.iter().filter(|call| matches!(call, Counted::Sum(_))).count();
        let stride = (1 + 2 * sums).next_power_of_two();
        Self {
            cells: vec![0; (width + 2) * stride],
            stride,
            sums,
            nulls: vec![Vec::new(); valid],
            outside: HashMap::new(),
        }
    }

    fn footprint(&self) -> usize {
        self.cells.len() * size_of::<i64>()
    }

    /// The rows counted at `place`.
    fn rows(&self, place: usize) -> i64 {
        self.cells[place * self.stride]
    }

    /// The total of sum `sum` at `place`.
    fn sum(&self, sum: usize, place: usize) -> i128 {
        let at = place * self.stride + 1 + 2 * sum;
        joined(self.cells[at], self.cells[at + 1])
    }

    /// The rows of `place` whose argument in the call counting into `lane` was not null.
    fn valid(&self, lane: usize, place: usize) -> i64 {
        self.rows(place) - self.nulls[lane].get(place).copied().unwrap_or(0)
    }

    fn add(&mut self, other: Self) -> Result<()> {
        let stride = self.stride;
        let places = self.cells.chunks_exact_mut(stride).zip(other.cells.chunks_exact(stride));
        for (into, from) in places {
            into[0] += from[0];
            for at in (1..1 + 2 * self.sums).step_by(2) {
                let total = joined(into[at], into[at + 1])
                    .checked_add(joined(from[at], from[at + 1]))
                    .ok_or_else(overflowed)?;
                split(total, &mut into[at..at + 2]);
            }
        }
        for (into, from) in self.nulls.iter_mut().zip(other.nulls) {
            if into.is_empty() {
                *into = from;
                continue;
            }
            for (into, from) in into.iter_mut().zip(from) {
                *into += from;
            }
        }
        for (key, counts) in other.outside {
            let held = self.outside.entry(key).or_insert_with(|| vec![(0, 0); counts.len()]);
            for (into, from) in held.iter_mut().zip(&counts) {
                into.0 += from.0;
                into.1 = into.1.checked_add(from.1).ok_or_else(overflowed)?;
            }
        }
        Ok(())
    }
}

impl Local {
    pub(crate) fn new(memory: &Memory) -> Self {
        Self {
            tallies: None,
            block: SignedBlock::default(),
            argument: SignedBlock::default(),
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
        let (mut nulls, mut sum) = (0, 0);
        let lanes = calls
            .iter()
            .map(|call| {
                let lane = Lane {
                    nulls: call.counts_valid().then_some(nulls),
                    sum: matches!(call, Counted::Sum(_)).then_some(sum),
                };
                nulls += usize::from(lane.nulls.is_some());
                sum += usize::from(lane.sum.is_some());
                lane
            })
            .collect();
        Self {
            key,
            low,
            width,
            calls,
            lanes,
            total: Mutex::new(None),
            held: Mutex::new(Vec::new()),
        }
    }

    /// Counts one chunk into this instance's arrays.
    ///
    /// `arguments` holds the one argument of each call, and nothing for a `COUNT(*)`, and each is
    /// `rows` long. A null argument reads as a zero, so a sum adds it without a branch and only
    /// the count of values beside it notices.
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
        let Local { tallies, block, argument: read, places, memory } = local;
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
        let stride = tallies.stride;
        for &place in places.iter() {
            tallies.cells[place as usize * stride] += 1;
        }
        for (lane, argument) in self.lanes.iter().zip(arguments) {
            let Some(nulls) = lane.nulls else { continue };
            let argument =
                argument.ok_or_else(|| Error::internal("a ranged call with no argument"))?;
            if let Some(sum) = lane.sum {
                let (cells, at) = (&mut tallies.cells, 1 + 2 * sum);
                if argument.logical_type().physical() == PhysicalType::Int128 {
                    wide_sum(argument, rows, places, cells, stride, at)?;
                } else {
                    read.read(rows, argument)?;
                    for (&place, &value) in places.iter().zip(read.cut(rows)?) {
                        let total = &mut cells[place as usize * stride + at..][..2];
                        split(joined(total[0], total[1]) + i128::from(value), total);
                    }
                }
            }
            if argument.none_null() {
                continue;
            }
            let into = &mut tallies.nulls[nulls];
            if into.is_empty() {
                memory.grow(u64::try_from((width + 2) * size_of::<i64>()).unwrap_or(u64::MAX))?;
                *into = vec![0; width + 2];
            }
            let validity = argument.validity();
            for (row, &place) in places.iter().enumerate() {
                into[place as usize] += i64::from(!validity.is_valid(row));
            }
        }
        // The rows the range did not cover, which should be none, counted again by their value.
        if tallies.rows(outside) != 0 {
            tallies.cells[outside * stride] = 0;
            for (row, &place) in places.iter().enumerate() {
                if place as usize != outside {
                    continue;
                }
                let mut counts = Vec::with_capacity(self.calls.len());
                for (call, argument) in self.calls.iter().zip(arguments) {
                    counts.push(match (call, argument) {
                        (Counted::Rows, _) | (_, None) => (1, 0),
                        (Counted::Valid, Some(argument)) => {
                            (i64::from(argument.validity().is_valid(row)), 0)
                        }
                        (Counted::Sum(_), Some(argument)) => match argument.signed_at(row) {
                            Some(value) => (1, value),
                            None => (0, 0),
                        },
                    });
                }
                let held = tallies
                    .outside
                    .entry(values[row])
                    .or_insert_with(|| vec![(0, 0); counts.len()]);
                for (into, from) in held.iter_mut().zip(counts) {
                    into.0 += from.0;
                    into.1 = into.1.checked_add(from.1).ok_or_else(overflowed)?;
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
            Some(held) => held.add(tallies)?,
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
        for place in 0..=self.width {
            let rows = tallies.rows(place);
            if rows == 0 {
                continue;
            }
            let key = (place < self.width).then(|| self.low + place as i64);
            out.keys.push(key);
            for (call, lane) in self.lanes.iter().enumerate() {
                let count = lane.nulls.map_or(rows, |nulls| tallies.valid(nulls, place));
                let sum = lane.sum.map_or(0, |sum| tallies.sum(sum, place));
                out.counts[call].push((count, sum));
            }
            if out.keys.len() == VECTOR_SIZE {
                chunks.push(out.chunk(&self.key, &self.calls, &mut charge)?);
            }
        }
        let mut outside: Vec<(i64, Vec<(i64, i128)>)> = tallies.outside.into_iter().collect();
        outside.sort_unstable_by_key(|(key, _)| *key);
        for (key, counts) in outside {
            out.keys.push(Some(key));
            for (call, count) in counts.into_iter().enumerate() {
                out.counts[call].push(count);
            }
            if out.keys.len() == VECTOR_SIZE {
                chunks.push(out.chunk(&self.key, &self.calls, &mut charge)?);
            }
        }
        if !out.keys.is_empty() {
            chunks.push(out.chunk(&self.key, &self.calls, &mut charge)?);
        }
        let mut held = self.held.lock().map_err(poisoned)?;
        held.clear();
        held.push(charge);
        timing.stop(0);
        Ok(chunks)
    }
}

/// The groups of the chunk being built, each call's state as its count and its sum.
struct Out {
    keys: Vec<Option<i64>>,
    counts: Vec<Vec<(i64, i128)>>,
}

impl Out {
    fn new(calls: usize) -> Self {
        Self { keys: Vec::with_capacity(VECTOR_SIZE), counts: vec![Vec::new(); calls] }
    }

    fn chunk(
        &mut self,
        ty: &LogicalType,
        calls: &[Counted],
        charge: &mut Reservation,
    ) -> Result<Chunk> {
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
        let mut bytes = rows * size_of::<i64>();
        for (counts, call) in self.counts.iter_mut().zip(calls) {
            let counts = std::mem::take(counts);
            columns.push(match call {
                Counted::Sum(returns) => {
                    bytes += rows * size_of::<i128>();
                    let sums = counts.iter().map(|&(_, sum)| sum).collect::<Vec<_>>();
                    let sums = Vector::flat(returns.clone(), Data::Int128(sums.into()))?;
                    if counts.iter().all(|&(count, _)| count != 0) {
                        sums
                    } else {
                        sums.with_validity(Validity::from_iter(rows, |row| counts[row].0 != 0))
                    }
                }
                _ => {
                    bytes += rows * size_of::<i64>();
                    let counts = counts.into_iter().map(|(count, _)| count).collect::<Vec<_>>();
                    Vector::flat(LogicalType::BigInt, Data::Int64(counts.into()))?
                }
            });
        }
        self.keys.clear();
        charge.grow(u64::try_from(bytes).unwrap_or(u64::MAX))?;
        Chunk::with_rows(columns, rows)
    }
}

/// Adds a column of 128 bit values into the sums at their places, checking every add. Each place's
/// total is the two cells `at` into its `stride`.
///
/// A flat column with no nulls is read as the slice it is. Any other form is asked a row at a time,
/// where a null answers nothing and adds nothing.
fn wide_sum(
    argument: &Vector,
    rows: usize,
    places: &[u32],
    cells: &mut [i64],
    stride: usize,
    at: usize,
) -> Result<()> {
    let mut add = |place: u32, value: i128| {
        let total = &mut cells[place as usize * stride + at..][..2];
        split(joined(total[0], total[1]).checked_add(value).ok_or_else(overflowed)?, total);
        Ok::<_, Error>(())
    };
    if let (Some(Data::Int128(values)), true) = (argument.data(), argument.none_null()) {
        let values = values
            .as_slice()
            .get(..rows)
            .ok_or_else(|| Error::internal("a wide sum was read short of the chunk"))?;
        for (&place, &value) in places.iter().zip(values) {
            add(place, value)?;
        }
        return Ok(());
    }
    for (row, &place) in places.iter().enumerate() {
        let value = match argument.signed_at(row) {
            Some(value) => value,
            None if argument.is_null_at(row) => continue,
            None => return Err(Error::internal("a wide sum has no signed representation")),
        };
        add(place, value)?;
    }
    Ok(())
}

/// The 128 bit total two cells hold, the low half first.
#[expect(clippy::cast_sign_loss, reason = "the low half is its bits")]
fn joined(low: i64, high: i64) -> i128 {
    i128::from(high) << 64 | i128::from(low as u64)
}

/// `total` written into the two cells of `into`, the low half first.
#[expect(clippy::cast_possible_truncation, reason = "each half is 64 of its bits")]
fn split(total: i128, into: &mut [i64]) {
    into[0] = total as i64;
    into[1] = (total >> 64) as i64;
}

fn overflowed() -> Error {
    Error::out_of_range("Overflow in the running total of a sum")
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

    /// A count of values is the rows less the nulls, whichever instance saw the nulls, and an
    /// instance that saw none adds its rows to one that did.
    #[test]
    fn a_count_of_values_is_the_rows_less_the_nulls_across_instances() {
        let memory = Memory::unlimited();
        let exchange =
            Exchange::new(LogicalType::Integer, 0, 3, vec![Counted::Valid, Counted::Rows]);
        let key = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(0), Value::Integer(2), Value::Integer(0)],
        )
        .expect("keys");
        let full = Vector::from_values(
            LogicalType::BigInt,
            &[Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)],
        )
        .expect("arguments");
        let holed =
            Vector::from_values(LogicalType::BigInt, &[Value::Null, Value::BigInt(2), Value::Null])
                .expect("arguments");
        for argument in [&full, &holed, &full] {
            let mut local = Local::new(&memory);
            exchange.count(&key, &[Some(argument), None], 3, &mut local).expect("counts");
            exchange.combine(local).expect("combines");
        }
        let int = Value::Integer;
        let big = Value::BigInt;
        assert_eq!(
            answer(&exchange, &memory),
            vec![vec![int(0), big(4), big(6)], vec![int(2), big(3), big(3)]]
        );
    }

    /// A sum answers the unscaled total in the type it was given, null for a group whose values
    /// were all null, and a value the range does not cover is summed beside the arrays.
    #[test]
    fn a_sum_answers_null_for_a_group_of_nulls_and_adds_across_instances() {
        let memory = Memory::unlimited();
        let returns = LogicalType::decimal(38, 2).expect("a decimal");
        let exchange = Exchange::new(
            LogicalType::Integer,
            1,
            3,
            vec![Counted::Sum(returns.clone()), Counted::Rows],
        );
        let key = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Integer(1), Value::Integer(7)],
        )
        .expect("keys");
        let price = LogicalType::decimal(15, 2).expect("a decimal");
        let decimal = |unscaled| Value::Decimal { unscaled, width: 15, scale: 2 };
        let argument = Vector::from_values(
            price,
            &[decimal(150), Value::Null, decimal(999_999_999_999_999), decimal(-5)],
        )
        .expect("arguments");
        for _ in 0..2 {
            let mut local = Local::new(&memory);
            exchange.count(&key, &[Some(&argument), None], 4, &mut local).expect("counts");
            exchange.combine(local).expect("combines");
        }
        let sum = |unscaled| Value::Decimal { unscaled, width: 38, scale: 2 };
        let big = Value::BigInt;
        let int = Value::Integer;
        assert_eq!(
            answer(&exchange, &memory),
            vec![
                vec![int(1), sum(300 + 2 * 999_999_999_999_999), big(4)],
                vec![int(2), Value::Null, big(2)],
                vec![int(7), sum(-10), big(2)],
            ]
        );
    }

    /// Two sums and a count share a place's cells, and a total that crosses zero or runs past 64
    /// bits carries between the two halves the same within an instance and across two.
    #[test]
    fn two_sums_beside_a_count_carry_between_halves_across_instances() {
        let memory = Memory::unlimited();
        let returns = LogicalType::decimal(38, 2).expect("a decimal");
        let exchange = Exchange::new(
            LogicalType::Integer,
            0,
            2,
            vec![Counted::Sum(returns.clone()), Counted::Rows, Counted::Sum(returns)],
        );
        let key = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(0), Value::Integer(1), Value::Integer(0), Value::Integer(1)],
        )
        .expect("keys");
        let narrow = LogicalType::decimal(15, 2).expect("a decimal");
        let small = |unscaled| Value::Decimal { unscaled, width: 15, scale: 2 };
        let first = Vector::from_values(narrow, &[small(3), small(-7), small(-1), small(2)])
            .expect("arguments");
        let wide = LogicalType::decimal(34, 2).expect("a decimal");
        let large = |unscaled| Value::Decimal { unscaled, width: 34, scale: 2 };
        let far = 1_i128 << 70;
        let second = Vector::from_values(wide, &[large(far), large(-1), large(-far - 1), large(far)])
            .expect("arguments");
        for _ in 0..2 {
            let mut local = Local::new(&memory);
            exchange
                .count(&key, &[Some(&first), None, Some(&second)], 4, &mut local)
                .expect("counts");
            exchange.combine(local).expect("combines");
        }
        let sum = |unscaled| Value::Decimal { unscaled, width: 38, scale: 2 };
        assert_eq!(
            answer(&exchange, &memory),
            vec![
                vec![Value::Integer(0), sum(4), Value::BigInt(4), sum(-2)],
                vec![Value::Integer(1), sum(-10), Value::BigInt(4), sum(2 * far - 2)],
            ]
        );
    }

    /// A 128 bit argument, which is how q11's product is stored, is added as it is, skips its
    /// nulls, and an add past the largest 128 bit value is an error rather than a wrapped total.
    #[test]
    fn a_wide_sum_skips_nulls_and_refuses_to_overflow() {
        let memory = Memory::unlimited();
        let returns = LogicalType::decimal(38, 2).expect("a decimal");
        let exchange = Exchange::new(LogicalType::Integer, 0, 2, vec![Counted::Sum(returns)]);
        let key = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(0), Value::Integer(0), Value::Integer(1)],
        )
        .expect("keys");
        let wide = LogicalType::decimal(34, 2).expect("a decimal");
        let decimal = |unscaled| Value::Decimal { unscaled, width: 34, scale: 2 };
        let argument = Vector::from_values(wide.clone(), &[decimal(7), Value::Null, decimal(-3)])
            .expect("arguments");
        let mut local = Local::new(&memory);
        exchange.count(&key, &[Some(&argument)], 3, &mut local).expect("counts");
        exchange.combine(local).expect("combines");
        let sum = |unscaled| Value::Decimal { unscaled, width: 38, scale: 2 };
        assert_eq!(
            answer(&exchange, &memory),
            vec![vec![Value::Integer(0), sum(7)], vec![Value::Integer(1), sum(-3)]]
        );

        let big = i128::MAX / 2 + 1;
        let argument = Vector::from_values(wide, &[decimal(big), decimal(big), decimal(0)])
            .expect("arguments");
        let mut local = Local::new(&memory);
        assert!(exchange.count(&key, &[Some(&argument)], 3, &mut local).is_err());
    }
}
