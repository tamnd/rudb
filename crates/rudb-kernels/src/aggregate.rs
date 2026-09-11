//! The aggregate accumulators.
//!
//! One of these per group per aggregate, so a `GROUP BY` over a million distinct keys with three
//! aggregates in it holds three million of them. That is the reason the state is an enum of small
//! fixed cases rather than a boxed trait object: the hash table is going to hold these inline and a
//! pointer chase per row per aggregate is a cost that shows up on every grouped query there is.
//!
//! Null is skipped by every aggregate. `sum` over a column that is entirely null is null and not
//! zero, `count(x)` counts the rows where `x` is not null, and `count(*)` counts rows without
//! looking at anything. Those three are not variations on a theme, they are three different
//! questions, and the reason `count(*)` is a separate function rather than `count` with a star
//! argument is so the executor never has to work out which one it was handed.
//!
//! # How the vectorized path is put together
//!
//! The other four kernel files take a vector and give one back. This one has state, so the batch
//! interface is [`Accumulator::update_run`], which folds a whole vector into the running state in
//! one pass. What it can do in one pass depends on the aggregate, and the four shapes are worth
//! naming because they are not the same problem.
//!
//! `count` and `count(*)` do not read the data at all. A count of rows is the row count and a count
//! of values is the number of bits set in the validity mask, so those two are answered from the
//! mask whatever the form and whatever the type, which is why they are the only ones that come back
//! before the form dispatch.
//!
//! A whole sum reads the data and ignores the order. Rows that are null are masked out with a
//! conditional move rather than a branch, and the accumulator is an `i128` so that nothing narrower
//! than a `HUGEINT` can overflow inside one vector and the check only has to happen once, where the
//! vector's total meets the running total.
//!
//! A floating point sum reads the data and does not ignore the order, because floating point
//! addition is not associative and the answer this has to reach is the one the row at a time loop
//! reaches. So that loop stays sequential and gives up the vectorization the whole sum gets. It is
//! still about fifty times faster than building a `Value` per row, and an answer that is fast and
//! different from the reference is not an answer.
//!
//! `min` and `max` read the data to find which row won and then ask the vector for that one row.
//! One `Value` per vector instead of one per row, and one call into the comparison kernel instead
//! of one per row.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, Validity, Vector};

use crate::compare::order;
use crate::fallback::{self, Kernel};
use crate::number::{fit, integral, pow10, rescale};
use crate::shape::{identity, nulls_of};

/// A running aggregate.
#[derive(Debug, Clone)]
pub struct Accumulator {
    kind: Kind,
    returns: LogicalType,
    state: State,
}

/// Which aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// What the aggregate has seen so far.
#[derive(Debug, Clone)]
enum State {
    /// A row count, for `count` and `count(*)`.
    Counted(i64),
    /// A whole running total and whether anything landed in it.
    Whole { total: i128, seen: bool },
    /// A running total in floating point, and the count `avg` divides by.
    Real { total: f64, seen: i64 },
    /// A running total at a fixed decimal scale.
    Scaled { total: i128, scale: u8, seen: bool },
    /// The smallest or largest value so far.
    Extreme(Option<Value>),
}

impl Accumulator {
    /// A fresh accumulator for a named aggregate returning `returns`.
    ///
    /// # Errors
    ///
    /// If the name is not an aggregate this crate implements.
    pub fn new(name: &str, returns: &LogicalType) -> Result<Self> {
        let kind = match name {
            "count_star" => Kind::CountStar,
            "count" => Kind::Count,
            "sum" => Kind::Sum,
            "avg" => Kind::Avg,
            "min" => Kind::Min,
            "max" => Kind::Max,
            other => {
                return Err(Error::not_implemented(format!("the {other} aggregate")));
            }
        };
        let state = match kind {
            Kind::CountStar | Kind::Count => State::Counted(0),
            Kind::Avg => State::Real { total: 0.0, seen: 0 },
            Kind::Min | Kind::Max => State::Extreme(None),
            Kind::Sum => match returns {
                LogicalType::Decimal { scale, .. } => {
                    State::Scaled { total: 0, scale: *scale, seen: false }
                }
                LogicalType::Float | LogicalType::Double => State::Real { total: 0.0, seen: 0 },
                _ => State::Whole { total: 0, seen: false },
            },
        };
        Ok(Self { kind, returns: returns.clone(), state })
    }

    /// Folds one row in.
    ///
    /// # Errors
    ///
    /// If the argument count is wrong for the aggregate, if the value is not one the aggregate can
    /// accumulate, or if a whole running total overflows.
    pub fn update(&mut self, args: &[Value]) -> Result<()> {
        if self.kind == Kind::CountStar {
            if let State::Counted(count) = &mut self.state {
                *count += 1;
            }
            return Ok(());
        }
        let value = match args {
            [only] => only,
            _ => {
                return Err(Error::internal(format!("an aggregate over {} arguments", args.len())));
            }
        };
        if value.is_null() {
            return Ok(());
        }
        match &mut self.state {
            State::Counted(count) => *count += 1,
            State::Whole { total, seen } => {
                let whole = integral(value).ok_or_else(|| not_summable(value))?;
                *total = total.checked_add(whole).ok_or_else(overflowed)?;
                *seen = true;
            }
            State::Real { total, seen } => {
                *total += approximate_or_error(value)?;
                *seen += 1;
            }
            State::Scaled { total, scale, seen } => {
                let unscaled = at_scale(value, *scale).ok_or_else(|| not_summable(value))?;
                *total = total.checked_add(unscaled).ok_or_else(overflowed)?;
                *seen = true;
            }
            State::Extreme(held) => {
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let ordering = order(value, current)?;
                        match self.kind {
                            Kind::Min => ordering.is_lt(),
                            _ => ordering.is_gt(),
                        }
                    }
                };
                if replace {
                    *held = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    /// Folds a whole vector in, in one pass over the data and without building a [`Value`] per row.
    ///
    /// `rows` is how many rows to fold, which is the row count of the chunk rather than the capacity
    /// of the vectors in it. `count(*)` takes no argument and reads nothing but that number, and
    /// every other aggregate here takes exactly one vector.
    ///
    /// A shape the one pass form does not cover falls through to [`Accumulator::update`] per row and
    /// records itself in [`crate::fallback`], so this always reaches the answer the row at a time
    /// loop reaches and never a different one. That is not a slogan about floating point: the
    /// running total below is carried into the vector loop rather than restarted at zero, precisely
    /// so that the additions happen in the same order and round the same way.
    ///
    /// # Errors
    ///
    /// The same errors [`Accumulator::update`] raises, for the same reasons.
    pub fn update_run(&mut self, args: &[Vector], rows: usize) -> Result<()> {
        if self.kind == Kind::CountStar {
            if let State::Counted(count) = &mut self.state {
                *count += i64::try_from(rows).map_err(|_| overlong())?;
            }
            return Ok(());
        }
        let input = match args {
            [only] => only,
            _ => {
                return Err(Error::internal(format!("an aggregate over {} arguments", args.len())));
            }
        };
        if input.len() < rows {
            return Err(Error::internal(format!(
                "an aggregate handed {rows} rows and a vector of {}",
                input.len()
            )));
        }
        if self.folded(input, rows)? {
            return Ok(());
        }
        // An aggregate reads one vector, so its form goes in both halves of the report rather than
        // leaving a column of zeros next to every row of it.
        fallback::record(Kernel::Aggregate, input.form(), input.form());
        for row in 0..rows {
            let value = input.value_at(row);
            self.update(std::slice::from_ref(&value))?;
        }
        Ok(())
    }

    /// Folds a vector in in one pass, or says this is a shape the one pass form does not cover.
    ///
    /// # Errors
    ///
    /// If a whole running total overflows where the row at a time loop would also have overflowed.
    fn folded(&mut self, input: &Vector, rows: usize) -> Result<bool> {
        let nulls = nulls_of(input);
        // Both counts are answered by the mask on its own, whatever the form is and whatever the
        // type is, so they come back before there is any question of which loop to run.
        if let State::Counted(count) = &mut self.state {
            *count += i64::try_from(nulls.count_valid(rows)).map_err(|_| overlong())?;
            return Ok(true);
        }
        let least = self.kind == Kind::Min;
        let want = match (&self.state, input.logical_type()) {
            (State::Whole { .. }, _) => Want::Whole,
            (State::Real { total, .. }, ty) => {
                Want::Real { scale: decimal_scale(ty), from: *total }
            }
            // A total at the scale the column is already held at is a sum of the raw unscaled
            // integers and nothing else, which is the case every real query is in, because the sum
            // of a `DECIMAL(15, 2)` column is declared at scale two. An integer summed into a
            // decimal total, or a decimal at some other scale, needs a rescale per row that the row
            // at a time path already does correctly, so those go that way and the counter says
            // whether that was the wrong call.
            (State::Scaled { scale, .. }, LogicalType::Decimal { scale: held, .. })
                if held == scale =>
            {
                Want::Whole
            }
            (State::Scaled { .. }, _) => return Ok(false),
            (State::Extreme(_), _) => Want::Extreme(least),
            (State::Counted(_), _) => return Ok(false),
        };
        let Some(contribution) = gather(input, rows, &nulls, want) else {
            return Ok(false);
        };
        let live = nulls.count_valid(rows);
        match (&mut self.state, contribution) {
            (
                State::Whole { total, seen } | State::Scaled { total, seen, .. },
                Contribution::Whole(sum),
            ) => {
                *total = total.checked_add(sum).ok_or_else(overflowed)?;
                *seen |= live > 0;
            }
            (State::Real { total, seen }, Contribution::Real { total: carried, seen: added }) => {
                *total = carried;
                *seen += added;
            }
            (State::Extreme(held), Contribution::Extreme(Some(index))) => {
                // One `Value` for the whole vector and one call into the comparison kernel, rather
                // than one of each per row. The row that won is found on the numbers.
                let candidate = input.value_at(index);
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let ordering = order(&candidate, current)?;
                        if least { ordering.is_lt() } else { ordering.is_gt() }
                    }
                };
                if replace {
                    *held = Some(candidate);
                }
            }
            (State::Extreme(_), Contribution::Extreme(None)) => {}
            // The `want` above picks the contribution, so the pairs left over are ones that cannot
            // be built. Falling through costs a slow loop and a wrong answer costs a lot more.
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The aggregate's answer.
    ///
    /// # Errors
    ///
    /// If the running total does not fit the declared return type.
    pub fn finish(&self) -> Result<Value> {
        match &self.state {
            State::Counted(count) => Ok(Value::BigInt(*count)),
            State::Whole { total, seen } => {
                if !seen {
                    return Ok(Value::Null);
                }
                fit(*total, &self.returns).ok_or_else(|| {
                    Error::out_of_range(format!(
                        "a sum of {total} does not fit in {}",
                        self.returns
                    ))
                })
            }
            State::Real { total, seen } => {
                if *seen == 0 {
                    return Ok(Value::Null);
                }
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let answer = if self.kind == Kind::Avg { total / *seen as f64 } else { *total };
                if matches!(self.returns, LogicalType::Float) {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a declared FLOAT result is a FLOAT"
                    )]
                    return Ok(Value::Float(answer as f32));
                }
                Ok(Value::Double(answer))
            }
            State::Scaled { total, scale, seen } => {
                if !seen {
                    return Ok(Value::Null);
                }
                let width = match self.returns {
                    LogicalType::Decimal { width, .. } => width,
                    _ => rudb_common::MAX_DECIMAL_WIDTH,
                };
                Ok(Value::Decimal { unscaled: *total, width, scale: *scale })
            }
            State::Extreme(held) => Ok(held.clone().unwrap_or(Value::Null)),
        }
    }
}

fn not_summable(value: &Value) -> Error {
    Error::not_implemented(format!("summing a {}", value.logical_type()))
}

fn approximate_or_error(value: &Value) -> Result<f64> {
    crate::number::approximate(value).ok_or_else(|| not_summable(value))
}

/// A value as an unscaled integer at a fixed scale.
fn at_scale(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } => rescale(unscaled, held, scale),
        _ => integral(value).and_then(|whole| whole.checked_mul(pow10(scale))),
    }
}

fn overflowed() -> Error {
    Error::out_of_range("Overflow in the running total of a sum".to_string())
}

fn overlong() -> Error {
    Error::out_of_range("more rows in one vector than a count can hold".to_string())
}

/// The scale a type holds its numbers at, which is zero for everything that is not a decimal.
fn decimal_scale(ty: &LogicalType) -> u8 {
    match *ty {
        LogicalType::Decimal { scale, .. } => scale,
        _ => 0,
    }
}

/// What one vector has to be read for.
#[derive(Clone, Copy)]
enum Want {
    /// A total of exact numbers, read at whatever scale they are already held at.
    Whole,
    /// A total in floating point, carried on from what is already there rather than restarted.
    Real { scale: u8, from: f64 },
    /// The row that wins, the smallest one if true and the largest one if false.
    Extreme(bool),
}

/// What one vector contributes.
enum Contribution {
    Whole(i128),
    Real { total: f64, seen: i64 },
    Extreme(Option<usize>),
}

/// Reads a vector once, whichever form it is in.
fn gather(input: &Vector, rows: usize, nulls: &Validity, want: Want) -> Option<Contribution> {
    match input.form() {
        Form::Flat => {
            let data = input.data()?;
            if data.len() < rows {
                return None;
            }
            collect(data, identity, rows, nulls, want)
        }
        Form::Dictionary => {
            let (codes, values) = input.dictionary_parts()?;
            if codes.len() < rows {
                return None;
            }
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather below indexes without a bound of its own.
            collect(values.data()?, |index| codes[index] as usize, rows, nulls, want)
        }
        // A constant folds in as one value repeated and a sequence as an arithmetic series, and
        // both have a closed form that is better than any loop. Neither is what a scan of a column
        // produces, so both wait for the counter to ask for them.
        _ => None,
    }
}

fn collect<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
    want: Want,
) -> Option<Contribution> {
    match want {
        Want::Whole => whole_sum(data, at, rows, nulls).map(Contribution::Whole),
        Want::Real { scale, from } => real_sum(data, at, rows, nulls, scale, from),
        Want::Extreme(least) => extreme(data, at, rows, nulls, least).map(Contribution::Extreme),
    }
}

/// The total of the rows that are not null, as an exact number.
///
/// The accumulator is an `i128` and the widest thing read into it is sixty four bits, so a vector
/// would have to be about 2^63 rows long before its own total could overflow. That is what lets the
/// only overflow check be the one where this total meets the running total, which in turn is what
/// lets the loop vectorize at all.
fn whole_sum<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
) -> Option<i128> {
    macro_rules! summed {
        ($values:expr) => {{
            let values = $values;
            let mut total: i128 = 0;
            match nulls {
                Validity::AllValid => {
                    for index in 0..rows {
                        total += i128::from(values[at(index)]);
                    }
                }
                Validity::AllInvalid => {}
                Validity::Mask(mask) => {
                    // A word of the mask at a time, and a conditional move rather than a branch
                    // inside it, because the rows a filter leaves behind are in no pattern a branch
                    // predictor is going to learn.
                    for start in (0..rows).step_by(64) {
                        let word = mask.word(start / 64);
                        for index in start..(start + 64).min(rows) {
                            let number = i128::from(values[at(index)]);
                            total += if word >> (index - start) & 1 == 1 { number } else { 0 };
                        }
                    }
                }
            }
            total
        }};
    }
    Some(match data {
        Data::Int8(values) => summed!(values),
        Data::Int16(values) => summed!(values),
        Data::Int32(values) => summed!(values),
        Data::Int64(values) => summed!(values),
        Data::UInt8(values) => summed!(values),
        Data::UInt16(values) => summed!(values),
        Data::UInt32(values) => summed!(values),
        Data::UInt64(values) => summed!(values),
        // A total of hugeints can overflow inside one vector, and then the overflow is the answer
        // rather than a detail. That one goes the row at a time way, which is the way that raises.
        _ => return None,
    })
}

/// The running total carried through the rows that are not null, in floating point.
///
/// Sequential on purpose. Floating point addition is not associative, so four accumulators or a
/// reassociation would give an answer that is close to the one the row at a time loop gives rather
/// than the same one, and this file's whole job is to be the thing the fast paths are checked
/// against. What it does buy is the `Value` per row, the enum match per row and the null check per
/// row, and that is most of the cost.
#[expect(
    clippy::cast_precision_loss,
    reason = "a wide integer past 2^53 losing digits is what a double is, and this is the float path"
)]
fn real_sum<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
    scale: u8,
    from: f64,
) -> Option<Contribution> {
    let factor = pow10(scale) as f64;
    let scaled = scale != 0;
    let all = i64::try_from(rows).ok()?;
    macro_rules! added {
        ($values:expr, $convert:expr) => {{
            let values = $values;
            let convert = $convert;
            let mut total = from;
            let mut seen: i64 = 0;
            // Which rows count is decided once for the vector rather than once per row. A match on
            // the validity enum inside the loop is three quarters of a nanosecond a row on top of
            // an addition that takes one, which is a thing worth finding out by measuring.
            match nulls {
                Validity::AllValid => {
                    for index in 0..rows {
                        let number = convert(values[at(index)]);
                        total += if scaled { number / factor } else { number };
                    }
                    seen = all;
                }
                Validity::AllInvalid => {}
                Validity::Mask(mask) => {
                    for start in (0..rows).step_by(64) {
                        let word = mask.word(start / 64);
                        for index in start..(start + 64).min(rows) {
                            if word >> (index - start) & 1 == 0 {
                                continue;
                            }
                            let number = convert(values[at(index)]);
                            total += if scaled { number / factor } else { number };
                            seen += 1;
                        }
                    }
                }
            }
            (total, seen)
        }};
    }
    let (total, seen) = match data {
        Data::Int8(values) => added!(values, |number| f64::from(number)),
        Data::Int16(values) => added!(values, |number| f64::from(number)),
        Data::Int32(values) => added!(values, |number| f64::from(number)),
        Data::Int64(values) => added!(values, |number| number as f64),
        Data::Int128(values) => added!(values, |number| number as f64),
        Data::UInt8(values) => added!(values, |number| f64::from(number)),
        Data::UInt16(values) => added!(values, |number| f64::from(number)),
        Data::UInt32(values) => added!(values, |number| f64::from(number)),
        Data::UInt64(values) => added!(values, |number| number as f64),
        Data::UInt128(values) => added!(values, |number| number as f64),
        Data::Float32(values) => added!(values, |number| f64::from(number)),
        Data::Float64(values) => added!(values, |number: f64| number),
        _ => return None,
    };
    Some(Contribution::Real { total, seen })
}

/// Which row holds the smallest or largest number, or none if every row is null.
fn extreme<M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
    least: bool,
) -> Option<Option<usize>> {
    macro_rules! best {
        ($values:expr) => {{
            let values = $values;
            // The winner is a row number and a number, not an `Option` of a pair. Carrying the
            // option into the loop puts a discriminant test on every row, and the first row is the
            // only row that needs one, so the seed is the first row that is not null and the loop
            // starts after it.
            let mut held = usize::MAX;
            let mut mark: i128 = 0;
            match nulls {
                Validity::AllValid => {
                    if rows > 0 {
                        mark = i128::from(values[at(0)]);
                        held = 0;
                        for index in 1..rows {
                            let number = i128::from(values[at(index)]);
                            let win = if least { number < mark } else { number > mark };
                            if win {
                                mark = number;
                                held = index;
                            }
                        }
                    }
                }
                Validity::AllInvalid => {}
                Validity::Mask(mask) => {
                    for start in (0..rows).step_by(64) {
                        let word = mask.word(start / 64);
                        for index in start..(start + 64).min(rows) {
                            if word >> (index - start) & 1 == 0 {
                                continue;
                            }
                            let number = i128::from(values[at(index)]);
                            let win = if least { number < mark } else { number > mark };
                            if held == usize::MAX || win {
                                mark = number;
                                held = index;
                            }
                        }
                    }
                }
            }
            (held != usize::MAX).then_some(held)
        }};
    }
    Some(match data {
        Data::Int8(values) => best!(values),
        Data::Int16(values) => best!(values),
        Data::Int32(values) => best!(values),
        Data::Int64(values) => best!(values),
        Data::UInt8(values) => best!(values),
        Data::UInt16(values) => best!(values),
        Data::UInt32(values) => best!(values),
        Data::UInt64(values) => best!(values),
        // A float orders NaN the way the comparison kernel says rather than the way the hardware
        // does, and a string extreme is a comparison of bytes rather than of numbers. Both are
        // worth a loop of their own and neither gets a wrong one here.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, returns: &LogicalType, rows: &[Value]) -> Value {
        let mut accumulator = Accumulator::new(name, returns).expect("a known aggregate");
        for row in rows {
            accumulator.update(std::slice::from_ref(row)).expect("accumulates");
        }
        accumulator.finish().expect("finishes")
    }

    #[test]
    fn count_star_counts_rows_and_count_counts_values() {
        let mut stars = Accumulator::new("count_star", &LogicalType::BigInt).expect("known");
        for _ in 0..3 {
            stars.update(&[]).expect("no arguments");
        }
        assert_eq!(stars.finish().expect("finishes"), Value::BigInt(3));
        let counted = run(
            "count",
            &LogicalType::BigInt,
            &[Value::Integer(1), Value::Null, Value::Integer(3)],
        );
        assert_eq!(counted, Value::BigInt(2));
    }

    /// The distinction that makes `sum` over an empty group different from `count` over one.
    #[test]
    fn a_sum_of_nothing_is_null_and_a_count_of_nothing_is_zero() {
        assert_eq!(run("sum", &LogicalType::HugeInt, &[]), Value::Null);
        assert_eq!(run("sum", &LogicalType::HugeInt, &[Value::Null]), Value::Null);
        assert_eq!(run("count", &LogicalType::BigInt, &[]), Value::BigInt(0));
        assert_eq!(run("count_star", &LogicalType::BigInt, &[]), Value::BigInt(0));
    }

    #[test]
    fn a_sum_of_integers_accumulates_wider_than_it_reads() {
        let rows = vec![Value::Integer(i32::MAX); 4];
        let total = run("sum", &LogicalType::HugeInt, &rows);
        assert_eq!(total, Value::HugeInt(i128::from(i32::MAX) * 4));
    }

    #[test]
    fn an_average_divides_by_the_rows_it_saw_rather_than_the_rows_there_were() {
        let average =
            run("avg", &LogicalType::Double, &[Value::Integer(1), Value::Null, Value::Integer(3)]);
        assert_eq!(average, Value::Double(2.0));
    }

    #[test]
    fn min_and_max_skip_nulls_and_keep_the_value_rather_than_a_number() {
        let smallest = run(
            "min",
            &LogicalType::Varchar,
            &[Value::Varchar("b".into()), Value::Null, Value::Varchar("a".into())],
        );
        assert_eq!(smallest, Value::Varchar("a".into()));
        let largest = run(
            "max",
            &LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(7), Value::Integer(3)],
        );
        assert_eq!(largest, Value::Integer(7));
    }

    #[test]
    fn a_decimal_sums_at_its_own_scale() {
        let ty = LogicalType::decimal(10, 2).expect("a legal decimal");
        let total = run(
            "sum",
            &ty,
            &[
                Value::Decimal { unscaled: 250, width: 10, scale: 2 },
                Value::Decimal { unscaled: 125, width: 10, scale: 2 },
            ],
        );
        assert_eq!(total, Value::Decimal { unscaled: 375, width: 10, scale: 2 });
    }

    #[test]
    fn an_aggregate_nobody_has_written_says_which_one() {
        let error = Accumulator::new("median", &LogicalType::Double)
            .expect_err("median is not written yet");
        assert!(error.message().contains("the median aggregate"), "{error}");
    }

    /// The row at a time path, which is the answer the one pass path has to reach.
    fn row_at_a_time(name: &str, returns: &LogicalType, batches: &[Vector]) -> Result<Value> {
        let mut accumulator = Accumulator::new(name, returns)?;
        for batch in batches {
            for row in 0..batch.len() {
                let value = batch.value_at(row);
                accumulator.update(std::slice::from_ref(&value))?;
            }
        }
        accumulator.finish()
    }

    fn a_vector_at_a_time(name: &str, returns: &LogicalType, batches: &[Vector]) -> Result<Value> {
        let mut accumulator = Accumulator::new(name, returns)?;
        for batch in batches {
            accumulator.update_run(std::slice::from_ref(batch), batch.len())?;
        }
        accumulator.finish()
    }

    /// Both paths on the same batches, agreeing on the answer or agreeing on the complaint.
    fn agrees(name: &str, returns: &LogicalType, batches: &[Vector], note: &str) {
        let slow = row_at_a_time(name, returns, batches);
        let fast = a_vector_at_a_time(name, returns, batches);
        match (slow, fast) {
            (Ok(slow), Ok(fast)) => assert_eq!(slow, fast, "{note}"),
            (Err(slow), Err(fast)) => {
                assert_eq!(slow.message(), fast.message(), "{note}");
            }
            (slow, fast) => {
                panic!(
                    "{note}: one path answered and the other did not, {slow:?} against {fast:?}"
                );
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// A number small enough to be legal in every type below, so that the property test is about
    /// the loops rather than about which types happen to hold which ranges.
    fn small(rng: &mut Rng) -> i64 {
        (rng.next() % 201) as i64 - 100
    }

    fn sample(ty: &LogicalType, rng: &mut Rng) -> Value {
        let number = small(rng);
        let positive = number.unsigned_abs();
        match *ty {
            LogicalType::TinyInt => Value::TinyInt(number as i8),
            LogicalType::SmallInt => Value::SmallInt(number as i16),
            LogicalType::Integer => Value::Integer(number as i32),
            LogicalType::BigInt => Value::BigInt(number),
            LogicalType::HugeInt => Value::HugeInt(i128::from(number)),
            LogicalType::UTinyInt => Value::UTinyInt(positive as u8),
            LogicalType::USmallInt => Value::USmallInt(positive as u16),
            LogicalType::UInteger => Value::UInteger(positive as u32),
            LogicalType::UBigInt => Value::UBigInt(positive),
            LogicalType::Float => Value::Float(number as f32 / 8.0),
            LogicalType::Double => Value::Double(number as f64 / 8.0),
            LogicalType::Decimal { width, scale } => {
                Value::Decimal { unscaled: i128::from(number) * 7, width, scale }
            }
            LogicalType::Varchar => Value::Varchar(format!("w{number}")),
            _ => panic!("no sample for {ty}"),
        }
    }

    fn flat(ty: &LogicalType, rows: usize, nulls: usize, rng: &mut Rng) -> Vector {
        let values: Vec<Value> = (0..rows)
            .map(
                |index| {
                    if nulls > 0 && index % nulls == 0 { Value::Null } else { sample(ty, rng) }
                },
            )
            .collect();
        Vector::from_values(ty.clone(), &values).expect("a vector of this type")
    }

    /// What the declared return type is for an aggregate over a column of this type.
    fn returns_of(name: &str, ty: &LogicalType) -> LogicalType {
        match name {
            "count" | "count_star" => LogicalType::BigInt,
            "avg" => LogicalType::Double,
            "min" | "max" => ty.clone(),
            _ => match *ty {
                LogicalType::Decimal { scale, .. } => {
                    LogicalType::decimal(rudb_common::MAX_DECIMAL_WIDTH, scale)
                        .expect("the widest decimal at this scale is legal")
                }
                LogicalType::Float | LogicalType::Double => LogicalType::Double,
                _ => LogicalType::HugeInt,
            },
        }
    }

    /// Every aggregate over every type this crate knows, in both forms that have a loop and at
    /// three null densities, against the loop the loops replaced.
    #[test]
    fn every_aggregate_over_every_type_agrees_with_the_row_at_a_time_path() {
        // Some of the pairs below are shapes the one pass form refuses on purpose, and refusing
        // increments a process wide counter that other tests assert exact values of.
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        let mut rng = Rng(0x5eed_ca11_ab1e_0003);
        let types = [
            LogicalType::TinyInt,
            LogicalType::SmallInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UTinyInt,
            LogicalType::USmallInt,
            LogicalType::UInteger,
            LogicalType::UBigInt,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::decimal(9, 2).expect("a legal decimal"),
            LogicalType::decimal(18, 4).expect("a legal decimal"),
            LogicalType::decimal(30, 6).expect("a legal decimal"),
            LogicalType::Varchar,
        ];
        for ty in &types {
            for name in ["count_star", "count", "sum", "avg", "min", "max"] {
                let returns = returns_of(name, ty);
                for nulls in [0_usize, 4, 1] {
                    // Two batches rather than one, because a running total that is restarted at
                    // every vector is right on one vector and wrong on the query.
                    let first = flat(ty, 97, nulls, &mut rng);
                    let second = flat(ty, 64, nulls, &mut rng);
                    let note = format!("{name} over {ty}, flat, one null in {nulls}");
                    agrees(name, &returns, &[first.clone(), second.clone()], &note);
                    let codes: Vec<u32> = (0..97).map(|index| (index % 13) as u32).collect();
                    let coded = Vector::dictionary(codes, first).expect("codes are in range");
                    let note = format!("{name} over {ty}, dictionary, one null in {nulls}");
                    agrees(name, &returns, &[coded, second], &note);
                }
            }
        }
    }

    #[test]
    fn a_sum_of_numbers_stays_off_the_row_at_a_time_path_and_a_sum_of_strings_does_not() {
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        fallback::reset();
        let numbers = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        )
        .expect("a vector of integers");
        let mut summing =
            Accumulator::new("sum", &LogicalType::HugeInt).expect("a known aggregate");
        summing.update_run(std::slice::from_ref(&numbers), 3).expect("sums");
        assert_eq!(summing.finish().expect("finishes"), Value::HugeInt(6));
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 0);

        let words = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Null, Value::Varchar("b".into())],
        )
        .expect("a vector of strings");
        let mut counting = Accumulator::new("count", &LogicalType::BigInt).expect("a known one");
        counting.update_run(std::slice::from_ref(&words), 3).expect("counts");
        assert_eq!(counting.finish().expect("finishes"), Value::BigInt(2));
        // A count reads the mask, so a type with no loop of its own is still not a fall through.
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 0);

        let mut wrong = Accumulator::new("sum", &LogicalType::HugeInt).expect("a known aggregate");
        let error =
            wrong.update_run(std::slice::from_ref(&words), 3).expect_err("cannot sum those");
        assert!(error.message().contains("summing a"), "{error}");
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 1);
        fallback::reset();
    }

    /// The reason `update_run` carries the running total into the loop rather than totalling the
    /// vector on its own and adding the two at the end.
    #[test]
    fn a_floating_point_sum_carries_the_running_total_into_the_next_vector() {
        let first =
            Vector::from_values(LogicalType::Double, &[Value::Double(1.0e16)]).expect("a vector");
        let second = Vector::from_values(LogicalType::Double, &vec![Value::Double(1.0); 8])
            .expect("a vector");
        let batches = [first, second];
        let slow = row_at_a_time("sum", &LogicalType::Double, &batches).expect("sums");
        let fast = a_vector_at_a_time("sum", &LogicalType::Double, &batches).expect("sums");
        assert_eq!(slow, fast);
        // One at a time, every one of those eight disappears into the rounding. Eight at once does
        // not, which is what makes this a case worth having a test for.
        assert_eq!(slow, Value::Double(1.0e16));
        assert_ne!(1.0e16 + 8.0, 1.0e16);
    }

    /// The wrong answer that needs a dictionary, a null and one specific code to reproduce.
    #[test]
    fn a_null_behind_a_dictionary_code_is_skipped_by_every_aggregate() {
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Null, Value::Integer(5), Value::Integer(9)],
        )
        .expect("a vector of integers");
        let coded = Vector::dictionary(vec![0, 1, 0, 2, 0], values).expect("codes are in range");
        let batch = std::slice::from_ref(&coded);
        assert_eq!(
            a_vector_at_a_time("count", &LogicalType::BigInt, batch).expect("counts"),
            Value::BigInt(2)
        );
        assert_eq!(
            a_vector_at_a_time("sum", &LogicalType::HugeInt, batch).expect("sums"),
            Value::HugeInt(14)
        );
        assert_eq!(
            a_vector_at_a_time("min", &LogicalType::Integer, batch).expect("finds one"),
            Value::Integer(5)
        );
    }

    /// Why the whole sum stops at sixty four bits: at a hundred and twenty eight the total of one
    /// vector can overflow on its own, and the overflow is the answer rather than a detail.
    #[test]
    fn a_total_of_hugeints_goes_the_row_at_a_time_way_and_still_overflows() {
        let _turn = fallback::TURN.lock().expect("no test panics while holding this");
        fallback::reset();
        let rows = vec![Value::HugeInt(i128::MAX); 2];
        let vector = Vector::from_values(LogicalType::HugeInt, &rows).expect("a vector");
        let mut accumulator = Accumulator::new("sum", &LogicalType::HugeInt).expect("a known one");
        let error =
            accumulator.update_run(std::slice::from_ref(&vector), 2).expect_err("overflows");
        assert!(error.message().contains("Overflow in the running total"), "{error}");
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 1);
        fallback::reset();
    }
}
