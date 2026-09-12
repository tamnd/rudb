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
//!
//! # The grouped form
//!
//! [`update_run`](Accumulator::update_run) folds a vector into one accumulator, which is what an
//! ungrouped aggregate wants and is no use at all to a `GROUP BY`, where the rows of one vector
//! belong to as many different accumulators as there are groups in it. [`update_scattered`] is the
//! grouped form: one vector, one slot per row saying which accumulator that row belongs to, and one
//! pass that reads the run once and folds each value into the accumulator its row points at.
//!
//! It cannot reduce the way the ungrouped form does, because two adjacent rows are usually two
//! different groups and there is nothing to add up before the scatter. What it removes is everything
//! else: the `Value` built per row, which for a string column is a malloc, the match on which
//! aggregate this is, and the match on which layout the column is in. All three of those are decided
//! once per vector here and none of them per row.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Data, Form, Validity, Vector};

use crate::compare::order;
use crate::fallback::{self, Kernel};
use crate::number::{fit, integral, pow10, rescale};
use crate::shape::{identity, nulls_of};

/// Where a row that belongs to no accumulator points.
///
/// A row a `FILTER` threw away and a row that went to a spill file both have nothing to update, and
/// the caller says so by pointing them here rather than by handing over a second mask. One sentinel
/// rather than a second buffer, because the slots are written per row anyway and the test is a
/// compare against a constant.
pub const NOWHERE: usize = usize::MAX;

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
    /// A running total for `avg`, exact while every value folded in is a whole number.
    ///
    /// `avg` over an integer column has to add the column up exactly and divide once at the end.
    /// Adding into a double as it goes gives a different number: each addition past `2^53` rounds,
    /// and the roundings do not cancel. `AVG(UserID)` over ten thousand rows of the benchmark file
    /// came out `435091026172918.3` that way where duckdb says `435091026172920.25`, which is the
    /// sum divided once. So the total is an `i128` and the division is the only rounding.
    ///
    /// `exact` goes false the first time a value is not a whole number, or the first time the total
    /// would overflow, and from then on `real` carries it. A column of doubles therefore lands on
    /// the same additions in the same order as before, which is what the float path has to keep.
    Mean { whole: i128, real: f64, seen: i64, exact: bool },
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
            Kind::Avg => State::Mean { whole: 0, real: 0.0, seen: 0, exact: true },
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
                let whole = integral(value).ok_or_else(|| not_narrow(value))?;
                *total = total.checked_add(whole).ok_or_else(overflowed)?;
                *seen = true;
            }
            State::Real { total, seen } => {
                *total += approximate_or_error(value)?;
                *seen += 1;
            }
            State::Mean { whole, real, seen, exact } => {
                match integral(value)
                    .filter(|_| *exact)
                    .and_then(|number| whole.checked_add(number))
                {
                    Some(total) => *whole = total,
                    None => {
                        // The first value that is not whole, or the first one that would overflow.
                        // What was counted exactly so far comes across as one conversion, and the
                        // rest of the column is added the way it always was.
                        if *exact {
                            *real = exactly(*whole);
                            *exact = false;
                        }
                        *real += approximate_or_error(value)?;
                    }
                }
                *seen += 1;
            }
            State::Scaled { total, scale, seen } => {
                let unscaled = at_scale(value, *scale).ok_or_else(|| not_narrow(value))?;
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
        // row at a time: the path recorded on the line above, which exists to be correct for an
        // aggregate `folded` does not cover and counts itself so that aggregate shows up.
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
            // An exact mean over an integer column is read the way a sum is and divided at the end.
            // Anything else is the float path, carried on from wherever the total is now, which for
            // a mean that was exact until this vector is the exact total converted once.
            (State::Mean { exact: true, .. }, ty) if ty.is_integer() => Want::Whole,
            (State::Mean { whole, real, exact, .. }, ty) => {
                let from = if *exact { exactly(*whole) } else { *real };
                Want::Real { scale: decimal_scale(ty), from }
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
            (State::Mean { whole, seen, .. }, Contribution::Whole(sum)) => {
                // An overflow here is not an error the way it is for a sum, because the row at a
                // time loop answers an overflowing mean in floating point rather than raising. So
                // this hands the vector back and that loop folds it in, state untouched.
                let Some(total) = whole.checked_add(sum) else { return Ok(false) };
                *whole = total;
                *seen += i64::try_from(live).map_err(|_| overlong())?;
            }
            (
                State::Mean { real, seen, exact, .. },
                Contribution::Real { total: carried, seen: added },
            ) => {
                *real = carried;
                *exact = false;
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
            State::Mean { whole, real, seen, exact } => {
                if *seen == 0 {
                    return Ok(Value::Null);
                }
                let total = if *exact { exactly(*whole) } else { *real };
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let answer = total / *seen as f64;
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

/// Folds one vector into many accumulators, each row into the one its slot points at.
///
/// `states` is the caller's flat array of accumulators, `stride` is how many aggregates there are
/// per group and `offset` is which of them this call is, so the accumulator a row belongs to is at
/// `slots[row] * stride + offset`. That is the layout a hash aggregate already keeps, one run of
/// accumulators per group, so nothing is copied to call this and the slots are the probe results the
/// operator has in hand. A slot of [`NOWHERE`] is a row that contributes to nothing.
///
/// A shape the one pass form does not cover falls through to [`Accumulator::update`] per row and
/// records itself in [`crate::fallback`], exactly as [`Accumulator::update_run`] does, so this
/// always reaches the answer the row at a time loop reaches. The rows are walked in order in every
/// path, which is what keeps a floating point total inside one group adding in the same order and
/// rounding the same way.
///
/// # Errors
///
/// The same errors [`Accumulator::update`] raises, and an internal error if the slots are shorter
/// than the rows or if the vector is.
pub fn update_scattered(
    states: &mut [Accumulator],
    slots: &[usize],
    stride: usize,
    offset: usize,
    input: Option<&Vector>,
    rows: usize,
) -> Result<()> {
    if states.is_empty() {
        return Ok(());
    }
    if slots.len() < rows {
        return Err(Error::internal(format!(
            "an aggregate handed {rows} rows and {} slots",
            slots.len()
        )));
    }
    let Some(first) = states.get(offset) else {
        return Err(Error::internal(format!(
            "an aggregate at {offset} of {} accumulators",
            states.len()
        )));
    };
    let kind = first.kind;
    let into = Where { slots, stride, offset };
    // `count(*)` reads nothing, so it never asks for the argument it does not have.
    if kind == Kind::CountStar {
        for row in 0..rows {
            let Some(index) = into.index(row) else { continue };
            if let State::Counted(count) = &mut states[index].state {
                *count += 1;
            }
        }
        return Ok(());
    }
    let Some(input) = input else {
        return Err(Error::internal("an aggregate over 0 arguments".to_string()));
    };
    if input.len() < rows {
        return Err(Error::internal(format!(
            "an aggregate handed {rows} rows and a vector of {}",
            input.len()
        )));
    }
    let nulls = nulls_of(input);
    // Every aggregate here skips nulls, so a vector that is entirely null contributes nothing to
    // anything whatever the type is and whatever the form is.
    if matches!(nulls, Validity::AllInvalid) {
        return Ok(());
    }
    let feed = feed_of(first, input.logical_type());
    if let Some(feed) = feed {
        if spread(states, into, input, rows, &nulls, feed)? {
            return Ok(());
        }
    }
    // An aggregate reads one vector, so its form goes in both halves of the report.
    fallback::record(Kernel::Aggregate, input.form(), input.form());
    // row at a time: the path recorded on the line above, which exists to be correct for a column
    // `spread` does not cover and counts itself so that column shows up.
    for row in 0..rows {
        let Some(index) = into.index(row) else { continue };
        let value = input.value_at(row);
        states[index].update(std::slice::from_ref(&value))?;
    }
    Ok(())
}

/// Which accumulator a row belongs to.
#[derive(Clone, Copy)]
struct Where<'w> {
    slots: &'w [usize],
    stride: usize,
    offset: usize,
}

impl Where<'_> {
    /// The accumulator this row folds into, or none if it folds into nothing.
    fn index(self, row: usize) -> Option<usize> {
        let slot = self.slots[row];
        (slot != NOWHERE).then(|| slot * self.stride + self.offset)
    }
}

/// What one run of values is read as on the way into many accumulators.
#[derive(Clone, Copy)]
enum Feed {
    /// A count of the rows that are not null, which reads the mask and not the data.
    Counted,
    /// An exact number per row.
    Whole,
    /// A number per row in floating point, at the scale a decimal column is held at.
    Real { scale: u8 },
    /// A number per row against the best that group has seen, the smallest one if true.
    Extreme(bool),
}

/// How a column is read for a call, or none if the one pass form does not cover it.
///
/// This is [`Accumulator::folded`]'s `want` with the running total left out, because there is no one
/// running total here. It has to make the same choices for the same reasons, so the arms are in the
/// same order and the comments there are the comments here.
fn feed_of(first: &Accumulator, ty: &LogicalType) -> Option<Feed> {
    match (&first.state, ty) {
        (State::Counted(_), _) => Some(Feed::Counted),
        (State::Whole { .. }, _) => Some(Feed::Whole),
        (State::Scaled { scale, .. }, LogicalType::Decimal { scale: held, .. })
            if held == scale =>
        {
            Some(Feed::Whole)
        }
        (State::Scaled { .. }, _) => None,
        (State::Mean { .. }, ty) if ty.is_integer() => Some(Feed::Whole),
        (State::Mean { .. } | State::Real { .. }, ty) => {
            Some(Feed::Real { scale: decimal_scale(ty) })
        }
        // A number is compared as a number and anything else is compared the way the comparison
        // kernel says, which a run of `i128` cannot do for a float, a string or a date.
        (State::Extreme(_), ty) if ty.is_integer() => Some(Feed::Extreme(first.kind == Kind::Min)),
        (State::Extreme(_), _) => None,
    }
}

/// One pass over a vector, folding each row into the accumulator it belongs to.
fn spread(
    states: &mut [Accumulator],
    into: Where<'_>,
    input: &Vector,
    rows: usize,
    nulls: &Validity,
    feed: Feed,
) -> Result<bool> {
    // Both counts are answered by the mask on its own, whatever the form and whatever the type, so
    // they come back before there is any question of which loop to run.
    if matches!(feed, Feed::Counted) {
        for row in 0..rows {
            if !nulls.is_valid(row) {
                continue;
            }
            let Some(index) = into.index(row) else { continue };
            if let State::Counted(count) = &mut states[index].state {
                *count += 1;
            }
        }
        return Ok(true);
    }
    match input.form() {
        Form::Flat => {
            let Some(data) = input.data() else { return Ok(false) };
            if data.len() < rows {
                return Ok(false);
            }
            let run = Run { input, data, rows, nulls };
            scatter(states, into, &run, identity, feed)
        }
        Form::Dictionary | Form::Rle => {
            let Some((codes, values)) = input.positions() else { return Ok(false) };
            if codes.len() < rows {
                return Ok(false);
            }
            let Some(data) = values.data() else { return Ok(false) };
            let run = Run { input, data, rows, nulls };
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather below indexes without a bound of its own.
            scatter(states, into, &run, |index| codes[index] as usize, feed)
        }
        // A constant and a sequence both have a closed form per group that is better than any loop,
        // and neither is what a scan of a column produces, so both wait for the counter to ask.
        _ => Ok(false),
    }
}

/// One vector to read, in the shape the three loops below all want it.
struct Run<'r> {
    input: &'r Vector,
    data: &'r Data,
    rows: usize,
    nulls: &'r Validity,
}

fn scatter<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
    feed: Feed,
) -> Result<bool> {
    match feed {
        Feed::Counted => Ok(true),
        Feed::Whole => whole_into(states, into, run, at),
        Feed::Real { scale } => real_into(states, into, run, at, scale),
        Feed::Extreme(least) => extreme_into(states, into, run, at, least),
    }
}

/// An exact number per row into the running total of the group that row belongs to.
fn whole_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
) -> Result<bool> {
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => {
                    for row in 0..run.rows {
                        if !run.nulls.is_valid(row) {
                            continue;
                        }
                        let Some(index) = into.index(row) else { continue };
                        fold_whole(&mut states[index], i128::from(values[at(row)]))?;
                    }
                })+
                // A total of hugeints can overflow inside one group, and then the overflow is the
                // answer rather than a detail, so both of those go the row at a time way.
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(narrow, each);
    Ok(true)
}

/// One exact number into one accumulator, which is [`Accumulator::update`] with the `Value` gone.
fn fold_whole(into: &mut Accumulator, number: i128) -> Result<()> {
    match &mut into.state {
        State::Whole { total, seen } | State::Scaled { total, seen, .. } => {
            *total = total.checked_add(number).ok_or_else(overflowed)?;
            *seen = true;
        }
        State::Mean { whole, real, seen, exact } => {
            match whole.checked_add(number).filter(|_| *exact) {
                Some(total) => *whole = total,
                None => {
                    if *exact {
                        *real = exactly(*whole);
                        *exact = false;
                    }
                    *real += exactly(number);
                }
            }
            *seen += 1;
        }
        // `feed_of` chose this loop off the state, so the states left over cannot be here.
        other => {
            return Err(Error::internal(format!("an exact total into {other:?}")));
        }
    }
    Ok(())
}

/// A number per row in floating point into the running total of the group that row belongs to.
#[expect(
    clippy::cast_precision_loss,
    reason = "a wide integer past 2^53 losing digits is what a double is, and this is the float path"
)]
fn real_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
    scale: u8,
) -> Result<bool> {
    let factor = pow10(scale) as f64;
    let scaled = scale != 0;
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => each!(@run values, |number| number as f64),)+
                Data::Float32(values) => each!(@run values, f64::from),
                Data::Float64(values) => each!(@run values, |number: f64| number),
                _ => return Ok(false),
            }
        };
        (@run $values:expr, $convert:expr) => {{
            let values = $values;
            let convert = $convert;
            for row in 0..run.rows {
                if !run.nulls.is_valid(row) {
                    continue;
                }
                let Some(index) = into.index(row) else { continue };
                let number = convert(values[at(row)]);
                fold_real(&mut states[index], if scaled { number / factor } else { number });
            }
        }};
    }
    rudb_vector::for_each_layout!(integer, each);
    Ok(true)
}

/// One approximate number into one accumulator.
fn fold_real(into: &mut Accumulator, number: f64) {
    match &mut into.state {
        State::Real { total, seen } => {
            *total += number;
            *seen += 1;
        }
        State::Mean { whole, real, seen, exact } => {
            // The first value that is not whole. What was counted exactly so far comes across as
            // one conversion, and the rest of the group is added the way the float path adds.
            if *exact {
                *real = exactly(*whole);
                *exact = false;
            }
            *real += number;
            *seen += 1;
        }
        // `feed_of` chose this loop off the state, so the states left over cannot be here.
        _ => {}
    }
}

/// A number per row against the best the group it belongs to has seen.
fn extreme_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
    least: bool,
) -> Result<bool> {
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => {
                    // row at a time: a scatter is per row by definition, since two adjacent rows
                    // are usually two different groups and there is nothing to reduce before it.
                    // The `Value` below is built on a win rather than on a row, which is the part
                    // that makes this loop worth having over the one it replaced.
                    for row in 0..run.rows {
                        if !run.nulls.is_valid(row) {
                            continue;
                        }
                        let Some(index) = into.index(row) else { continue };
                        let number = i128::from(values[at(row)]);
                        let State::Extreme(held) = &mut states[index].state else {
                            return Err(Error::internal("an extreme into a total".to_string()));
                        };
                        let replace = match held {
                            None => true,
                            Some(current) => {
                                let mark = integral(current).ok_or_else(|| not_narrow(current))?;
                                if least { number < mark } else { number > mark }
                            }
                        };
                        // The `Value` is built on a win and not per row, which for a column that
                        // arrives sorted is once and for a column that arrives shuffled is about
                        // the harmonic number of the rows in the group.
                        if replace {
                            *held = Some(run.input.value_at(row));
                        }
                    }
                })+
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(narrow, each);
    Ok(true)
}

fn not_narrow(value: &Value) -> Error {
    Error::not_implemented(format!("summing a {}", value.logical_type()))
}

/// An exact total as the double a mean divides, which is the one rounding `avg` over whole numbers
/// is allowed to do and is where duckdb does it too.
#[expect(
    clippy::cast_precision_loss,
    reason = "a total past 2^53 rounding once here is the definition of a double result"
)]
fn exactly(total: i128) -> f64 {
    total as f64
}

fn approximate_or_error(value: &Value) -> Result<f64> {
    crate::number::approximate(value).ok_or_else(|| not_narrow(value))
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
        Form::Dictionary | Form::Rle => {
            let (codes, values) = input.positions()?;
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
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => summed!(@run values),)+
                // A total of hugeints can overflow inside one vector, and then the overflow is the
                // answer rather than a detail. The `narrow` group is exactly the widths where it
                // cannot, so both hugeints are out of it and both go the row at a time way, which is
                // the way that raises.
                _ => return None,
            }
        };
        (@run $values:expr) => {{
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
    Some(rudb_vector::for_each_layout!(narrow, summed))
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
    // Every integer arm converts with `as`, which for the widths below `2^53` is the same value
    // `f64::from` gives and for the ones above it is the rounding this whole function is about.
    // Splitting the list in two so that the narrow half could say `from` would be two lists that
    // produce the same code, which is two chances to put a width in the wrong one.
    macro_rules! added {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => added!(@run values, |number| number as f64),)+
                Data::Float32(values) => added!(@run values, f64::from),
                Data::Float64(values) => added!(@run values, |number: f64| number),
                _ => return None,
            }
        };
        (@run $values:expr, $convert:expr) => {{
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
    let (total, seen) = rudb_vector::for_each_layout!(integer, added);
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
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => best!(@run values),)+
                // A float orders NaN the way the comparison kernel says rather than the way the
                // hardware does, and a string extreme is a comparison of bytes rather than of
                // numbers. Both are worth a loop of their own and neither gets a wrong one here.
                // The two hugeints are out because the seed and the running best are both `i128`.
                _ => return None,
            }
        };
        (@run $values:expr) => {{
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
    Some(rudb_vector::for_each_layout!(narrow, best))
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

    /// Four whole numbers that are all past 2^53, so the two ways of averaging them differ.
    ///
    /// The last one is what the benchmark's `UserID` column is made of and is the reason this test
    /// exists: `AVG(UserID)` came out `435091026172918.3` here where duckdb said
    /// `435091026172920.25`.
    const WIDE: [i64; 4] = [435090932899640449, 435090932899640450, 1000003, 999999999999999999];

    /// The mean of [`WIDE`] the way duckdb computes it, which is the sum and then one division.
    fn wide_mean() -> f64 {
        exactly(WIDE.iter().map(|&number| i128::from(number)).sum()) / 4.0
    }

    fn wide_values() -> Vec<Value> {
        WIDE.iter().map(|&number| Value::BigInt(number)).collect()
    }

    #[test]
    fn an_average_of_whole_numbers_adds_them_up_exactly_and_divides_once() {
        // Adding these into a double as they arrive rounds at every step and the roundings do not
        // cancel, so the running answer is off in the last digit. The assertion that the two ways
        // disagree is there because without it this test would pass on a build that never fixed
        // anything.
        let mut running = 0.0_f64;
        for value in wide_values() {
            running += crate::number::approximate(&value).expect("a number");
        }
        assert_ne!(running / 4.0, wide_mean(), "the two ways of averaging have to differ here");
        assert_eq!(run("avg", &LogicalType::Double, &wide_values()), Value::Double(wide_mean()));
    }

    #[test]
    fn the_vector_path_averages_whole_numbers_exactly_as_well() {
        let values = wide_values();
        let vector = Vector::from_values(LogicalType::BigInt, &values).expect("a vector of these");
        let mut accumulator = Accumulator::new("avg", &LogicalType::Double).expect("a known one");
        accumulator.update_run(std::slice::from_ref(&vector), values.len()).expect("folds them in");
        assert_eq!(accumulator.finish().expect("finishes"), Value::Double(wide_mean()));
    }

    /// A column that is not whole numbers is added the way it always was, in order, in a double.
    #[test]
    fn an_average_of_doubles_is_the_running_total_the_float_path_produces() {
        let rows = [Value::Double(1e17), Value::Double(1.0), Value::Double(3.0)];
        let mut running = 0.0_f64;
        for value in &rows {
            running += crate::number::approximate(value).expect("a number");
        }
        assert_eq!(run("avg", &LogicalType::Double, &rows), Value::Double(running / 3.0));
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

    /// Every aggregate over every type this crate knows, in all three forms that have a loop and
    /// at three null densities, against the loop the loops replaced.
    #[test]
    fn every_aggregate_over_every_type_agrees_with_the_row_at_a_time_path() {
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
                    let coded = Vector::dictionary(codes, first.clone()).expect("in range");
                    let note = format!("{name} over {ty}, dictionary, one null in {nulls}");
                    agrees(name, &returns, &[coded, second.clone()], &note);
                    // Runs of thirteen rows each, so a value the flat vector held once is read
                    // thirteen times and a run boundary lands inside a batch rather than on it.
                    let ends: Vec<u32> = (1..=8).map(|run| (run * 13).min(97)).collect();
                    let runs = Vector::runs(ends, first.slice(0, 8).expect("eight values"))
                        .expect("one value for each run");
                    let note = format!("{name} over {ty}, runs, one null in {nulls}");
                    agrees(name, &returns, &[runs, second], &note);
                }
            }
        }
    }

    /// How many aggregates a group holds in the test below, and which of them is the one measured.
    ///
    /// Not one and not the first one, because a stride of one and an offset of zero are the two
    /// values that make the index arithmetic right by accident.
    const STRIDE: usize = 3;
    const OFFSET: usize = 1;

    /// Which group each row belongs to, with some rows belonging to none.
    ///
    /// Round robin with a stride that is coprime with nothing in particular, so consecutive rows
    /// land in different groups, which is the case the scattered path exists for and the case a
    /// loop that quietly folded runs together would get wrong.
    fn deal(rows: usize, groups: usize) -> Vec<usize> {
        (0..rows).map(|row| if row % 11 == 5 { NOWHERE } else { (row * 7 + 3) % groups }).collect()
    }

    /// One accumulator per group fed a row at a time, which is the answer the scatter has to reach.
    fn group_at_a_time(
        name: &str,
        returns: &LogicalType,
        batches: &[(Vector, Vec<usize>)],
        groups: usize,
        reads: bool,
    ) -> Result<Vec<Value>> {
        let mut states = Vec::new();
        for _ in 0..groups {
            states.push(Accumulator::new(name, returns)?);
        }
        for (batch, slots) in batches {
            for (row, &slot) in slots.iter().enumerate() {
                if slot == NOWHERE {
                    continue;
                }
                if reads {
                    let value = batch.value_at(row);
                    states[slot].update(std::slice::from_ref(&value))?;
                } else {
                    states[slot].update(&[])?;
                }
            }
        }
        states.iter().map(Accumulator::finish).collect()
    }

    /// The same groups through one call per batch.
    fn group_at_once(
        name: &str,
        returns: &LogicalType,
        batches: &[(Vector, Vec<usize>)],
        groups: usize,
        reads: bool,
    ) -> Result<Vec<Value>> {
        let mut states = Vec::new();
        for _ in 0..groups * STRIDE {
            states.push(Accumulator::new(name, returns)?);
        }
        for (batch, slots) in batches {
            let input = reads.then_some(batch);
            update_scattered(&mut states, slots, STRIDE, OFFSET, input, slots.len())?;
        }
        (0..groups).map(|group| states[group * STRIDE + OFFSET].finish()).collect()
    }

    /// Every aggregate over every type, dealt out into five groups, against one accumulator each.
    ///
    /// This is the invariant the whole scattered path rests on and it is the same invariant the
    /// vector at a time test above asserts: a faster loop that reaches a different answer is not an
    /// answer. Two batches rather than one, because a state that is restarted at every vector is
    /// right on one vector and wrong on the query, and the slots change between them so no group
    /// sees the same rows twice.
    #[test]
    fn every_aggregate_scattered_into_groups_agrees_with_one_accumulator_per_group() {
        let mut rng = Rng(0x5eed_ca11_ab1e_0061);
        let groups = 5;
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
                let reads = name != "count_star";
                for nulls in [0_usize, 4, 1] {
                    let first = flat(ty, 97, nulls, &mut rng);
                    let second = flat(ty, 64, nulls, &mut rng);
                    let codes: Vec<u32> = (0..97).map(|index| (index % 13) as u32).collect();
                    let coded = Vector::dictionary(codes, first.clone()).expect("codes in range");
                    for (shape, batches) in [
                        ("flat", vec![first.clone(), second.clone()]),
                        ("dictionary", vec![coded, second.clone()]),
                    ] {
                        let dealt: Vec<(Vector, Vec<usize>)> = batches
                            .into_iter()
                            .map(|batch| {
                                let slots = deal(batch.len(), groups);
                                (batch, slots)
                            })
                            .collect();
                        let note = format!("{name} over {ty}, {shape}, one null in {nulls}");
                        let slow = group_at_a_time(name, &returns, &dealt, groups, reads);
                        let fast = group_at_once(name, &returns, &dealt, groups, reads);
                        match (slow, fast) {
                            (Ok(slow), Ok(fast)) => assert_eq!(slow, fast, "{note}"),
                            (Err(slow), Err(fast)) => {
                                assert_eq!(slow.message(), fast.message(), "{note}");
                            }
                            (slow, fast) => panic!(
                                "{note}: one path answered and the other did not, \
                                 {slow:?} against {fast:?}"
                            ),
                        }
                    }
                }
            }
        }
    }

    /// A row pointing at [`NOWHERE`] contributes to nothing, which is how a `FILTER` and a spilled
    /// row are both said. Counting is the aggregate that would notice a row it should not have seen.
    #[test]
    fn a_row_that_belongs_to_no_group_is_counted_by_nobody() {
        let column = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Integer(3), Value::Integer(4)],
        )
        .expect("a vector of integers");
        let mut states = vec![Accumulator::new("sum", &LogicalType::HugeInt).expect("known"); 2];
        let slots = [0, NOWHERE, 1, NOWHERE];
        update_scattered(&mut states, &slots, 1, 0, Some(&column), 4).expect("folds them in");
        assert_eq!(states[0].finish().expect("finishes"), Value::HugeInt(1));
        assert_eq!(states[1].finish().expect("finishes"), Value::HugeInt(3));
    }

    /// The shapes a grouped ClickBench query is made of stay off the row at a time path, and a
    /// column the scatter has no typed loop for goes down it and says so.
    #[test]
    fn the_shapes_a_grouped_query_is_made_of_stay_off_the_row_at_a_time_path() {
        let numbers = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Integer(3)],
        )
        .expect("a vector of integers");
        let words = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Varchar("c".into())],
        )
        .expect("a vector of strings");
        let slots = [0_usize, 1, 0];
        for (name, returns, column) in [
            ("count_star", LogicalType::BigInt, None),
            ("count", LogicalType::BigInt, Some(&numbers)),
            ("sum", LogicalType::HugeInt, Some(&numbers)),
            ("avg", LogicalType::Double, Some(&numbers)),
            ("min", LogicalType::Integer, Some(&numbers)),
            ("max", LogicalType::Integer, Some(&numbers)),
        ] {
            fallback::reset();
            let mut states = vec![Accumulator::new(name, &returns).expect("known"); 2];
            update_scattered(&mut states, &slots, 1, 0, column, 3).expect("folds them in");
            assert_eq!(
                fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat),
                0,
                "{name} over an integer column took the row at a time path"
            );
        }
        fallback::reset();
        let mut states = vec![Accumulator::new("min", &LogicalType::Varchar).expect("known"); 2];
        update_scattered(&mut states, &slots, 1, 0, Some(&words), 3).expect("folds them in");
        assert_eq!(states[0].finish().expect("finishes"), Value::Varchar("a".into()));
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 1);
    }

    /// The point of the shared accessor. A run length column goes down the same loop a dictionary
    /// does, so it does not reach the path that builds a `Value` a row, and the counter says so.
    #[test]
    fn a_sum_over_runs_takes_the_same_loop_a_dictionary_takes() {
        fallback::reset();
        let values = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(5), Value::Null, Value::Integer(7)],
        )
        .expect("a vector of integers");
        let runs = Vector::runs(vec![4, 6, 10], values).expect("one value for each run");
        assert_eq!(runs.form(), Form::Rle);
        let mut summing =
            Accumulator::new("sum", &LogicalType::HugeInt).expect("a known aggregate");
        summing.update_run(std::slice::from_ref(&runs), 10).expect("sums");
        // Four fives and four sevens, with the two nulls in the middle contributing nothing.
        assert_eq!(summing.finish().expect("finishes"), Value::HugeInt(48));
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Rle, Form::Rle), 0);
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 0);
        fallback::reset();
    }

    #[test]
    fn a_sum_of_numbers_stays_off_the_row_at_a_time_path_and_a_sum_of_strings_does_not() {
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
