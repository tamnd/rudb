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

use std::array;
use std::mem;
use std::sync::Arc;

use rudb_common::{Error, LogicalType, PhysicalType, Result, Value};
use rudb_vector::{Data, Form, Live, Validity, Vector};

use crate::arg_extreme::Key;
use crate::compare::order;
use crate::fallback::{self, Kernel};
use crate::general::General;
use crate::number::{fit, integral, pow10, rescale};
use crate::quantile::Column;
use crate::shape::{identity, nulls_of};

/// Where a row that belongs to no accumulator points.
///
/// A row a `FILTER` threw away and a row that went to a spill file both have nothing to update, and
/// the caller says so by pointing them here rather than by handing over a second mask. One sentinel
/// rather than a second buffer, because the slots are written per row anyway and the test is a
/// compare against a constant.
pub const NOWHERE: usize = usize::MAX;

/// The name an aggregate goes by when its call says which order to read its rows in, which is the
/// plain name followed by one pair of letters per sort key.
///
/// `list(x ORDER BY y DESC, z)` is `list ORDER BY dl,al`, where the first letter is the direction
/// and the second is where nulls go. The keys are the last arguments of the call, after the ones
/// the aggregate itself reads. Carrying the order in the name keeps it out of every rule that looks
/// at an aggregate by name, none of which knows what to do with an order, and every rule that walks
/// arguments sees the keys as arguments and keeps them.
#[must_use]
pub fn ordered_name(inner: &str, keys: &[(bool, bool)]) -> String {
    let keys: Vec<&str> = keys
        .iter()
        .map(|&(descending, nulls_first)| match (descending, nulls_first) {
            (false, false) => "al",
            (false, true) => "af",
            (true, false) => "dl",
            (true, true) => "df",
        })
        .collect();
    format!("{inner} ORDER BY {}", keys.join(","))
}

/// The plain name and the sort keys of a name [`ordered_name`] made, or `None` for any other name.
fn split_ordered(name: &str) -> Option<(&str, Vec<(bool, bool)>)> {
    let (inner, keys) = name.split_once(" ORDER BY ")?;
    let keys = keys
        .split(',')
        .map(|key| match key {
            "al" => Some((false, false)),
            "af" => Some((false, true)),
            "dl" => Some((true, false)),
            "df" => Some((true, true)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((inner, keys))
}

/// A running aggregate.
#[derive(Debug, Clone)]
pub struct Accumulator {
    state: State,
}

/// The part of an aggregate return type its final value needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Return {
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    HugeInt,
    UTinyInt,
    USmallInt,
    UInteger,
    UBigInt,
    UHugeInt,
    Float,
    Double,
    Decimal(u8),
    /// Min and max return their held value and do not inspect the declared type.
    Other,
}

impl Return {
    fn new(ty: &LogicalType) -> Self {
        match ty {
            LogicalType::TinyInt => Self::TinyInt,
            LogicalType::SmallInt => Self::SmallInt,
            LogicalType::Integer => Self::Integer,
            LogicalType::BigInt => Self::BigInt,
            LogicalType::HugeInt => Self::HugeInt,
            LogicalType::UTinyInt => Self::UTinyInt,
            LogicalType::USmallInt => Self::USmallInt,
            LogicalType::UInteger => Self::UInteger,
            LogicalType::UBigInt => Self::UBigInt,
            LogicalType::UHugeInt => Self::UHugeInt,
            LogicalType::Float => Self::Float,
            LogicalType::Double => Self::Double,
            LogicalType::Decimal { width, .. } => Self::Decimal(*width),
            _ => Self::Other,
        }
    }

    fn logical(self) -> LogicalType {
        match self {
            Self::TinyInt => LogicalType::TinyInt,
            Self::SmallInt => LogicalType::SmallInt,
            Self::Integer => LogicalType::Integer,
            Self::BigInt => LogicalType::BigInt,
            Self::HugeInt => LogicalType::HugeInt,
            Self::UTinyInt => LogicalType::UTinyInt,
            Self::USmallInt => LogicalType::USmallInt,
            Self::UInteger => LogicalType::UInteger,
            Self::UBigInt => LogicalType::UBigInt,
            Self::UHugeInt => LogicalType::UHugeInt,
            Self::Float => LogicalType::Float,
            Self::Double => LogicalType::Double,
            Self::Decimal(width) => LogicalType::Decimal { width, scale: 0 },
            Self::Other => LogicalType::Null,
        }
    }
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
    /// Anything in [`State::General`], which no vector path here reads.
    General,
}

/// What the aggregate has seen so far.
#[derive(Debug, Clone)]
enum State {
    /// A row count, for `count` and `count(*)`.
    Counted { count: i64, star: bool },
    /// A whole running total and whether anything landed in it.
    Whole { total: i128, seen: bool, returns: Return },
    /// A running total in floating point, and the count `avg` divides by.
    Real { total: f64, seen: i64, kind: Kind, returns: Return },
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
    ///
    /// A decimal column is whole numbers too. What it stores is an integer and the type says where
    /// the point goes, so `total` carries the sum of those integers and `scale` says how far to move
    /// the point once, at the end. That is the same argument the paragraph above makes for an
    /// integer column, and the alternative is what this used to do: turn each row into a double,
    /// divide it by a hundred and add that, which is a conversion and a division per row to reach a
    /// worse number than one division of an exact sum reaches. `scale` is zero for every type that
    /// is not a decimal, where dividing by one is what it has always done.
    // The exact and floating totals are mutually exclusive. The floating total's bits occupy the
    // same word as the exact i128 after `exact` becomes false, cutting every AVG state by 16 bytes.
    Mean { total: i128, seen: i64, exact: bool, scale: u8, returns: Return },
    /// A running total at a fixed decimal scale.
    Scaled { total: i128, scale: u8, seen: bool, returns: Return },
    /// The smallest or largest value so far.
    // A held value is boxed, since a Value is much wider than every numeric aggregate and most
    // ClickBench groups hold count, sum and avg states that should not each pay for one. A rank is
    // kept in place: it fits in the width the totals already take, and q29's MIN(Referer) over 400
    // thousand groups spent 26 MB and an allocation a group on boxes of one.
    Extreme { held: Option<Extremum>, least: bool },
    /// Any other aggregate, boxed so that the five above stay as narrow as they are.
    General(Box<General>),
}

/// An accumulator's answer as a number, before anything decides what type it is written as.
#[derive(Debug, Clone, Copy)]
enum Answer {
    /// No row contributed, so the answer is null whatever the column is.
    Null,
    /// A whole number, either an integer or the unscaled part of a decimal.
    Whole(i128),
    /// A floating point number.
    Real(f64),
}

/// Which run at a time finish a call takes, decided once per call from the state and the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Every answer is an [`Answer::Whole`] narrowed to the column's own integer width.
    Whole,
    /// Every answer is an [`Answer::Real`] written as a `DOUBLE`.
    Double,
    /// Every answer is an [`Answer::Real`] written as a `FLOAT`.
    Float,
}

/// What a min or a max is holding.
#[derive(Debug, Clone)]
enum Extremum {
    /// A value copied out of the column it came from.
    Held(Box<Value>),
    /// A position in a dictionary that knows its sorted order, kept in place of the value there.
    ///
    /// A grouped min over a column of strings does a comparison per row, and comparing two strings
    /// means fetching both out of a payload the size of the column. A dictionary that sorted its
    /// values when it was written already knows which of any two of them is smaller, so the
    /// comparison becomes two integers and a string is fetched once for the group that answers with
    /// it rather than once for every row that fails to.
    ///
    /// The dictionary is held rather than borrowed, and that is what makes the `Arc::ptr_eq` in
    /// [`Extremum::offer`] a sound way to ask whether two of these rank against each other. Two
    /// dictionaries number their values differently, so a rank from one means nothing to the other,
    /// and keeping this one alive is what stops a second from turning up at the same address and
    /// being taken for it.
    Ranked { dictionary: Arc<Vector>, code: u32, rank: u32 },
}

impl Extremum {
    /// The value this holds, read out of the dictionary when that is where it still is.
    fn value(&self) -> Result<Value> {
        match self {
            Self::Held(value) => Ok(Value::clone(value)),
            Self::Ranked { dictionary, code, .. } => dictionary.try_value_at(*code as usize),
        }
    }

    /// Replaces a rank with the value it stands for, so that something without ranks can compare.
    fn settle(&mut self) -> Result<&mut Value> {
        if let Self::Ranked { .. } = self {
            let value = self.value()?;
            *self = Self::Held(Box::new(value));
        }
        match self {
            Self::Held(value) => Ok(value),
            Self::Ranked { .. } => {
                Err(Error::internal("a settled extreme kept its rank".to_string()))
            }
        }
    }

    /// Offers one dictionary row, and keeps it when it beats what is already here.
    ///
    /// The whole point of the fast path is the first arm: same dictionary, so one integer compare
    /// and, on a win, two stores. Anything else settles into a value and compares the long way,
    /// which is what a mix of dictionaries or a mix of forms in one aggregate costs.
    fn offer(&mut self, dictionary: &Arc<Vector>, code: u32, rank: u32, least: bool) -> Result<()> {
        if let Self::Ranked { dictionary: mine, code: held_code, rank: held_rank } = self
            && Arc::ptr_eq(mine, dictionary)
        {
            if if least { rank < *held_rank } else { rank > *held_rank } {
                *held_code = code;
                *held_rank = rank;
            }
            return Ok(());
        }
        let candidate = dictionary.try_value_at(code as usize)?;
        let ordering = order(&candidate, self.settle()?)?;
        if if least { ordering.is_lt() } else { ordering.is_gt() } {
            *self = Self::Held(Box::new(candidate));
        }
        Ok(())
    }
}

impl Accumulator {
    /// The value of a COUNT state, and `None` for every other aggregate.
    #[must_use]
    pub fn counted(&self) -> Option<i64> {
        match self.state {
            State::Counted { count, .. } => Some(count),
            _ => None,
        }
    }

    /// Finish an exact integer SUM held in a compact grouped state.
    #[must_use]
    pub fn exact_sum(total: i128, seen: bool, returns: &LogicalType) -> Self {
        Self { state: State::Whole { total, seen, returns: Return::new(returns) } }
    }

    /// Finish an exact integer AVG held in a compact grouped state.
    ///
    /// The scale is zero because every caller of this has already checked that what it added up was
    /// an integer column. A decimal never reaches a compact state.
    #[must_use]
    pub fn exact_avg(total: i128, seen: i64, returns: &LogicalType) -> Self {
        Self {
            state: State::Mean {
                total,
                seen,
                exact: true,
                scale: 0,
                returns: Return::new(returns),
            },
        }
    }

    /// The exact whole total this state holds and whether anything landed in it, or `None` for a
    /// state that holds no exact total.
    ///
    /// A `sum` and an `avg` of the same column add the same numbers up, and an `avg` keeps the count
    /// it will divide by as well, so a query asking for both can fold the column once and read the
    /// sum out of the mean's state. This is that read. `None` for a mean that has gone inexact,
    /// which is a total that did not fit an `i128`, and for every state that never held a whole
    /// total, so that the caller has to say what it does about those rather than being given a
    /// number that is not one.
    #[must_use]
    pub fn exact_total(&self) -> Option<(i128, bool)> {
        match self.state {
            State::Mean { total, seen, exact: true, .. } => Some((total, seen > 0)),
            State::Whole { total, seen, .. } | State::Scaled { total, seen, .. } => {
                Some((total, seen))
            }
            _ => None,
        }
    }

    /// A `sum` over a column of this type, holding `total` already.
    ///
    /// Built through [`Accumulator::new`] and then written into, so that the state is the one a real
    /// `sum` over the same column would have kept rather than a second opinion about which one that
    /// is. The scale a decimal sum carries comes from `returns` the same way, which is what makes the
    /// total read out of a mean over the same column the right number to put here: both are the sum
    /// of the column's unscaled integers.
    ///
    /// # Errors
    ///
    /// A `returns` that gives `sum` a state holding no whole total, which is a caller asking for this
    /// over a column no exact sum exists for.
    pub fn sum_of(total: i128, seen: bool, returns: &LogicalType) -> Result<Self> {
        let mut held = Self::new("sum", returns)?;
        match &mut held.state {
            State::Whole { total: into, seen: saw, .. }
            | State::Scaled { total: into, seen: saw, .. } => {
                *into = total;
                *saw = seen;
            }
            _ => {
                return Err(Error::internal(format!(
                    "a sum over {returns} keeps no whole total to write into"
                )));
            }
        }
        Ok(held)
    }

    fn kind(&self) -> Kind {
        match self.state {
            State::Counted { star, .. } => {
                if star {
                    Kind::CountStar
                } else {
                    Kind::Count
                }
            }
            State::Whole { .. } | State::Scaled { .. } => Kind::Sum,
            State::Real { kind, .. } => kind,
            State::Mean { .. } => Kind::Avg,
            State::Extreme { least, .. } => {
                if least {
                    Kind::Min
                } else {
                    Kind::Max
                }
            }
            State::General(_) => Kind::General,
        }
    }

    fn returns(&self) -> Return {
        match self.state {
            State::Counted { .. } => Return::BigInt,
            State::Whole { returns, .. }
            | State::Real { returns, .. }
            | State::Mean { returns, .. }
            | State::Scaled { returns, .. } => returns,
            State::Extreme { .. } | State::General(_) => Return::Other,
        }
    }

    /// A fresh accumulator for a named aggregate returning `returns`.
    ///
    /// # Errors
    ///
    /// If the name is not an aggregate this crate implements.
    pub fn new(name: &str, returns: &LogicalType) -> Result<Self> {
        if let Some((inner, keys)) = split_ordered(name) {
            let inner = Box::new(Self::new(inner, returns)?);
            let general = General::Ordered { keys, rows: Vec::new(), inner };
            return Ok(Self { state: State::General(Box::new(general)) });
        }
        if let Some(general) = General::new(name, returns) {
            return Ok(Self { state: State::General(Box::new(general)) });
        }
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
        let scale = match returns {
            LogicalType::Decimal { scale, .. } => *scale,
            _ => 0,
        };
        let returns = Return::new(returns);
        let state = match kind {
            // Every name that builds one of these went to [`General::new`] above.
            Kind::General => return Err(Error::internal(format!("the {name} aggregate"))),
            Kind::CountStar | Kind::Count => {
                State::Counted { count: 0, star: kind == Kind::CountStar }
            }
            // The scale is the argument's and not the result's, and the argument is not known here,
            // so it arrives with the first vector or the first value folded in.
            Kind::Avg => State::Mean { total: 0, seen: 0, exact: true, scale: 0, returns },
            Kind::Min | Kind::Max => State::Extreme { held: None, least: kind == Kind::Min },
            Kind::Sum => match returns {
                Return::Decimal(_) => State::Scaled { total: 0, scale, seen: false, returns },
                Return::Float | Return::Double => {
                    State::Real { total: 0.0, seen: 0, kind, returns }
                }
                _ => State::Whole { total: 0, seen: false, returns },
            },
        };
        Ok(Self { state })
    }

    /// Folds one row in.
    ///
    /// # Errors
    ///
    /// If the argument count is wrong for the aggregate, if the value is not one the aggregate can
    /// accumulate, or if a whole running total overflows.
    pub fn update(&mut self, args: &[Value]) -> Result<()> {
        if let State::General(general) = &mut self.state {
            return general.update(args);
        }
        if self.kind() == Kind::CountStar {
            if let State::Counted { count, .. } = &mut self.state {
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
            State::Counted { count, .. } => *count += 1,
            State::Whole { total, seen, .. } => {
                let whole = integral(value).ok_or_else(|| not_narrow(value))?;
                *total = total.checked_add(whole).ok_or_else(overflowed)?;
                *seen = true;
            }
            State::Real { total, seen, .. } => {
                *total += approximate_or_error(value)?;
                *seen += 1;
            }
            State::Mean { total, seen, exact, scale, .. } => {
                // A decimal is the integer it is stored as, with the point put back once at the
                // end, so it counts as whole here exactly as an integer does. See [`State::Mean`].
                let whole = match *value {
                    Value::Decimal { unscaled, scale: held, .. } => {
                        *scale = held;
                        Some(unscaled)
                    }
                    _ => integral(value),
                };
                match whole.filter(|_| *exact).and_then(|number| total.checked_add(number)) {
                    Some(sum) => *total = sum,
                    None => {
                        // The first value that is not whole, or the first one that would overflow.
                        // What was counted exactly so far comes across as one conversion, and the
                        // rest of the column is added the way it always was. A decimal that got
                        // here adds its unscaled integer, since that is the unit the total is in.
                        let real = if *exact { exactly(*total) } else { mean_real(*total) };
                        let add = match whole {
                            Some(number) => exactly(number),
                            None => approximate_or_error(value)?,
                        };
                        *total = mean_bits(real + add);
                        *exact = false;
                    }
                }
                *seen += 1;
            }
            State::Scaled { total, scale, seen, .. } => {
                let unscaled = at_scale(value, *scale).ok_or_else(|| not_narrow(value))?;
                *total = total.checked_add(unscaled).ok_or_else(overflowed)?;
                *seen = true;
            }
            State::Extreme { held, least } => {
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let ordering = order(value, current.settle()?)?;
                        if *least { ordering.is_lt() } else { ordering.is_gt() }
                    }
                };
                if replace {
                    *held = Some(Extremum::Held(Box::new(value.clone())));
                }
            }
            // Taken at the top, before the null is skipped, and kept here for the match.
            State::General(general) => general.update(args)?,
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
        if let State::General(general) = &mut self.state {
            if general.takes_columns()
                && let Some(input) = args.first()
                && let Some(column) = Column::of(input, rows)
            {
                let valid = input.validity();
                for row in (0..rows).filter(|&row| valid.is_valid(row)) {
                    general.push_column(column, row, args)?;
                }
                return Ok(());
            }
            let keys = if general.keyed() { by_column(args, rows) } else { None };
            let mut row_args = Vec::with_capacity(args.len());
            for row in 0..rows {
                if let Some((column, valid)) = keys
                    && valid.is_valid(row)
                    && general.cannot_take(Key::at(column, row), args.len())
                {
                    continue;
                }
                row_args.clear();
                for arg in args {
                    row_args.push(arg.try_value_at(row)?);
                }
                general.update(&row_args)?;
            }
            return Ok(());
        }
        if self.kind() == Kind::CountStar {
            if let State::Counted { count, .. } = &mut self.state {
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
            let value = input.try_value_at(row)?;
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
        if let State::Counted { count, .. } = &mut self.state {
            *count += i64::try_from(nulls.count_valid(rows)).map_err(|_| overlong())?;
            return Ok(true);
        }
        let least = self.kind() == Kind::Min;
        // A min or a max over a dictionary that knows its sorted order is decided on ranks, and
        // nothing comes out of the payload until the answer is asked for. Without this the extreme
        // arm below picks a winning row per vector and reads the string at it, which over a hundred
        // million rows is ninety seven thousand point reads into a payload that keeps every block it
        // decodes. That is where `MIN(Referer) WHERE Referer <> ''` spent three gigabytes and half a
        // second, and it only showed up under a filter because without one every vector's winner is
        // the empty string and every one of those reads lands in the same block.
        if matches!(self.state, State::Extreme { .. })
            && ranked_extreme(&mut self.state, input, rows, &nulls, least)?
        {
            return Ok(true);
        }
        // A mean has to know the scale of what it is adding up, and the column is where that comes
        // from. It is set before the total is touched rather than alongside it, because a vector
        // that contributes nothing still says what the column is.
        if let (State::Mean { scale, .. }, LogicalType::Decimal { scale: held, .. }) =
            (&mut self.state, input.logical_type())
        {
            *scale = *held;
        }
        // Every arm that adds something up asks what the column is first. See [`addable`] for why
        // that is not the free check it looks like.
        let want = match (&self.state, input.logical_type()) {
            (State::Whole { .. }, ty) if ty.is_integer() => Want::Whole,
            (State::Real { total, .. }, ty) if addable(ty) => {
                Want::Real { scale: decimal_scale(ty), from: *total }
            }
            // An exact mean over an integer or a decimal column is read the way a sum is and
            // divided at the end. Anything else is the float path, carried on from wherever the
            // total is now, which for a mean that was exact until this vector is the exact total
            // converted once.
            (State::Mean { exact: true, .. }, ty)
                if ty.is_integer() || matches!(ty, LogicalType::Decimal { .. }) =>
            {
                Want::Whole
            }
            // The scale is zero whatever the column is, because the total a mean carries is already
            // in the units the column stores, and moving the point is the finish's job.
            (State::Mean { total, exact, .. }, ty) if addable(ty) => {
                let from = if *exact { exactly(*total) } else { mean_real(*total) };
                Want::Real { scale: 0, from }
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
            (State::Extreme { .. }, _) => Want::Extreme(least),
            // A scale that does not match, a count, and any total over a column that is not a
            // number. All three go to the row at a time loop, which is right about them.
            _ => return Ok(false),
        };
        let Some(contribution) = gather(input, rows, &nulls, want) else {
            return Ok(false);
        };
        let live = nulls.count_valid(rows);
        match (&mut self.state, contribution) {
            (
                State::Whole { total, seen, .. } | State::Scaled { total, seen, .. },
                Contribution::Whole(sum),
            ) => {
                *total = total.checked_add(sum).ok_or_else(overflowed)?;
                *seen |= live > 0;
            }
            (
                State::Real { total, seen, .. },
                Contribution::Real { total: carried, seen: added },
            ) => {
                *total = carried;
                *seen += added;
            }
            (State::Mean { total, seen, .. }, Contribution::Whole(sum)) => {
                // An overflow here is not an error the way it is for a sum, because the row at a
                // time loop answers an overflowing mean in floating point rather than raising. So
                // this hands the vector back and that loop folds it in, state untouched.
                let Some(sum) = total.checked_add(sum) else { return Ok(false) };
                *total = sum;
                *seen += i64::try_from(live).map_err(|_| overlong())?;
            }
            (
                State::Mean { total, seen, exact, .. },
                Contribution::Real { total: carried, seen: added },
            ) => {
                *total = mean_bits(carried);
                *exact = false;
                *seen += added;
            }
            (State::Extreme { held, .. }, Contribution::Extreme(Some(index))) => {
                // One `Value` for the whole vector and one call into the comparison kernel, rather
                // than one of each per row. The row that won is found on the numbers.
                let candidate = input.try_value_at(index)?;
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let ordering = order(&candidate, current.settle()?)?;
                        if least { ordering.is_lt() } else { ordering.is_gt() }
                    }
                };
                if replace {
                    *held = Some(Extremum::Held(Box::new(candidate)));
                }
            }
            (State::Extreme { .. }, Contribution::Extreme(None)) => {}
            // The `want` above picks the contribution, so the pairs left over are ones that cannot
            // be built. Falling through costs a slow loop and a wrong answer costs a lot more.
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Folds another accumulator over the same aggregate into this one.
    ///
    /// This is what lets one aggregate run on more than one thread. Each thread builds its own
    /// table, and where two of them found the same group there are two states for it, so the merge
    /// needs a way to add one state to another that does not go back to the rows either of them was
    /// built from. That is this, and it is the whole of what an aggregate has to provide to be
    /// parallel, because the key side of the merge is the same probe the fold already does.
    ///
    /// # A sum of doubles is not associative
    ///
    /// Two threads that split a column of doubles add it up in a different order than one thread
    /// does, and floating point addition does not care that the numbers are the same. So `sum` and
    /// `avg` over a `FLOAT` or a `DOUBLE` can answer differently depending on how the work was
    /// divided. That is not a bug being introduced here, it is the arithmetic, and it is the reason
    /// `avg` over integers keeps an exact `i128` total and divides once, which makes the common case
    /// of the two give the same answer whatever the division was.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::Internal`] if the two states are not the same aggregate over the
    /// same type, which is a caller merging two tables that did not come from the same operator.
    /// [`rudb_common::ErrorCode::OutOfRange`] if a whole running total overflows, which is the same
    /// answer the row at a time path gives to the same sum.
    pub fn combine(&mut self, other: &Self) -> Result<()> {
        match (&mut self.state, &other.state) {
            (State::Counted { count, star }, State::Counted { count: added, star: same })
                if star == same =>
            {
                *count += added;
            }
            (State::Whole { total, seen, .. }, State::Whole { total: added, seen: any, .. }) => {
                *total = total.checked_add(*added).ok_or_else(overflowed)?;
                *seen |= any;
            }
            (
                State::Scaled { total, scale, seen, .. },
                State::Scaled { total: added, scale: same, seen: any, .. },
            ) if scale == same => {
                *total = total.checked_add(*added).ok_or_else(overflowed)?;
                *seen |= any;
            }
            (State::Real { total, seen, .. }, State::Real { total: added, seen: more, .. }) => {
                *total += added;
                *seen += more;
            }
            (
                State::Mean { total, seen, exact, scale, .. },
                State::Mean { total: added, seen: more, exact: whole, scale: from, .. },
            ) => {
                // Two states over one call are over one column, so where both have folded a row
                // they agree about the scale. Where this one has not, it has no scale of its own
                // and takes the one the other learned.
                if *seen == 0 {
                    *scale = *from;
                }
                // Both sides exact and the sum still fitting is the case worth keeping exact,
                // because it is `AVG` over an integer column and it is what gives the same answer
                // however the rows were divided. Anything else falls to the floating total, and it
                // falls once rather than per row, so the side that was exact is converted here and
                // the two are added as doubles.
                let both = if *exact && *whole { total.checked_add(*added) } else { None };
                match both {
                    Some(sum) => *total = sum,
                    None => {
                        let here = if *exact { exactly(*total) } else { mean_real(*total) };
                        let there = if *whole { exactly(*added) } else { mean_real(*added) };
                        *total = mean_bits(here + there);
                        *exact = false;
                    }
                }
                *seen += more;
            }
            (State::Extreme { held, least }, State::Extreme { held: candidate, least: same })
                if least == same =>
            {
                if let Some(candidate) = candidate {
                    match (held.as_mut(), candidate) {
                        (None, _) => *held = Some(candidate.clone()),
                        // Two workers over one column hold ranks out of the one dictionary, so the
                        // merge that brings their tables together compares integers too.
                        (
                            Some(Extremum::Ranked { dictionary, rank, .. }),
                            Extremum::Ranked { dictionary: theirs, rank: other, .. },
                        ) if Arc::ptr_eq(dictionary, theirs) => {
                            if if *least { other < rank } else { other > rank } {
                                *held = Some(candidate.clone());
                            }
                        }
                        (Some(current), _) => {
                            let value = candidate.value()?;
                            let ordering = order(&value, current.settle()?)?;
                            if if *least { ordering.is_lt() } else { ordering.is_gt() } {
                                *held = Some(candidate.clone());
                            }
                        }
                    }
                }
            }
            (State::General(general), State::General(other)) => general.combine(other)?,
            (here, there) => {
                return Err(Error::internal(format!(
                    "combining a {here:?} aggregate state with a {there:?} one, which are not the \
                     same aggregate over the same type"
                )));
            }
        }
        Ok(())
    }

    /// The answer of an aggregate with no `GROUP BY`, which is [`Accumulator::finish`] except for
    /// `fsum` and `favg`. The pin combines an ungrouped state into an empty one before finishing
    /// it, and for those two that step changes the last digits.
    ///
    /// # Errors
    ///
    /// If the running total does not fit the declared return type.
    pub fn finish_ungrouped(&self) -> Result<Value> {
        if let State::General(general) = &self.state {
            return general.finish_ungrouped();
        }
        self.finish()
    }

    /// The aggregate's answer.
    ///
    /// # Errors
    ///
    /// If the running total does not fit the declared return type.
    pub fn finish(&self) -> Result<Value> {
        // The arithmetic is in `answer` so that the run at a time finish below reaches the same
        // number by the same route. What is left here is turning that number into a `Value`, which
        // is the part the run at a time finish exists to skip.
        if let State::General(general) = &self.state {
            return general.finish();
        }
        let Some(answer) = self.answer() else {
            let State::Extreme { held, .. } = &self.state else {
                return Err(Error::internal(
                    "an accumulator with no number and no extreme in it".to_string(),
                ));
            };
            return held.as_ref().map_or(Ok(Value::Null), Extremum::value);
        };
        match answer {
            Answer::Null => Ok(Value::Null),
            Answer::Whole(total) => match &self.state {
                State::Counted { count, .. } => Ok(Value::BigInt(*count)),
                State::Scaled { scale, .. } => {
                    let width = match self.returns() {
                        Return::Decimal(width) => width,
                        _ => rudb_common::MAX_DECIMAL_WIDTH,
                    };
                    Ok(Value::Decimal { unscaled: total, width, scale: *scale })
                }
                _ => {
                    let returns = self.returns().logical();
                    fit(total, &returns).ok_or_else(|| {
                        Error::out_of_range(format!("a sum of {total} does not fit in {}", returns))
                    })
                }
            },
            Answer::Real(answer) => {
                if self.returns() == Return::Float {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a declared FLOAT result is a FLOAT"
                    )]
                    return Ok(Value::Float(answer as f32));
                }
                Ok(Value::Double(answer))
            }
        }
    }

    /// The answer as a number, for the run at a time finish, or `None` for a state that owns one.
    ///
    /// This is [`Accumulator::finish`] with the `Value` left off the end of it, and the two share
    /// every line that decides what the number is so that they cannot drift apart. `Whole` carries
    /// an answer that belongs in an integer or an unscaled decimal, `Real` one that belongs in a
    /// float, and the caller narrows either one to the column it is writing.
    fn answer(&self) -> Option<Answer> {
        match &self.state {
            State::Counted { count, .. } => Some(Answer::Whole(i128::from(*count))),
            State::Whole { total, seen, .. } => {
                Some(if *seen { Answer::Whole(*total) } else { Answer::Null })
            }
            State::Real { total, seen, .. } => {
                if *seen == 0 {
                    return Some(Answer::Null);
                }
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let answer = if self.kind() == Kind::Avg { total / *seen as f64 } else { *total };
                Some(Answer::Real(answer))
            }
            State::Mean { total, seen, exact, scale, .. } => {
                if *seen == 0 {
                    return Some(Answer::Null);
                }
                let total = if *exact { exactly(*total) } else { mean_real(*total) };
                Some(Answer::Real(divide_mean(total, *seen, *scale)))
            }
            State::Scaled { total, seen, .. } => {
                Some(if *seen { Answer::Whole(*total) } else { Answer::Null })
            }
            State::Extreme { .. } | State::General(_) => None,
        }
    }

    /// Which run at a time finish, if any, this state can take, given the column being written.
    ///
    /// The answer depends only on the state's shape and the column's type, and every state of one
    /// call has the same shape, so the caller asks once per call rather than once per group.
    fn route(&self, ty: &LogicalType) -> Option<Route> {
        let returns = Return::new(ty);
        match &self.state {
            // COUNT declares BIGINT and `finish` answers BIGINT without consulting the type, so the
            // run path takes it only where the two already agree.
            State::Counted { .. } => (returns == Return::BigInt).then_some(Route::Whole),
            State::Whole { returns: held, .. } => (returns == *held).then_some(Route::Whole),
            // The state holds the sum of the stored integers and the column stores integers at its
            // own scale, so the two scales have to be the same one for the total to go in as it is.
            State::Scaled { returns: held, scale, .. } => match ty {
                LogicalType::Decimal { scale: column, .. }
                    if column == scale && returns == *held =>
                {
                    Some(Route::Whole)
                }
                _ => None,
            },
            State::Real { returns: held, .. } | State::Mean { returns: held, .. } => {
                match (returns, held) {
                    (Return::Float, Return::Float) => Some(Route::Float),
                    (Return::Double, Return::Double) => Some(Route::Double),
                    _ => None,
                }
            }
            State::Extreme { .. } | State::General(_) => None,
        }
    }

    /// Finishes an integer sum after adding one constant for every nonnull input row.
    pub fn finish_offset(&self, offset: i64, rows: i64) -> Result<Value> {
        let Value::HugeInt(total) = self.finish()? else {
            return Ok(Value::Null);
        };
        let added = i128::from(offset).checked_mul(i128::from(rows)).ok_or_else(overflowed)?;
        Ok(Value::HugeInt(total.checked_add(added).ok_or_else(overflowed)?))
    }
}

/// Finishes one call's states into one vector, without building a `Value` per group.
///
/// `at` is the groups to emit, in the order they are to be emitted, and the state for one of them is
/// at `slot * stride + offset`, which is the same layout [`update_scattered`] folds into. The result
/// is a flat vector of `ty` with as many rows as there are slots.
///
/// The row at a time alternative is what this replaces and it is what the caller falls back to when
/// this returns `Ok(None)`. That path asks each accumulator for a `Value`, pushes it into a vector
/// of values, and then hands the whole run to `Vector::from_values`, which pushes every value again
/// into the flat data, reads them all back once more to build the validity mask, and drops them.
/// Four passes and an owning tagged value per group per call to move eight bytes. Measured on TPC-H
/// SF1 in `spec/perf/14-what-a-group-costs.md`, that was the single largest thing that grew when a
/// grouped aggregate got another call, at 0.479 G of the 3.248 G two added calls cost.
///
/// What is left here is one pass. The route is decided once from the first state and the column
/// type, the answers go into a run of numbers, and the run is narrowed into the column's own layout.
/// Nothing is allocated per group and nothing is dropped per group.
///
/// `Ok(None)` is a shape this does not cover, which is a `min` or a `max`, whose state owns a value
/// away from itself, and any call whose declared return type is not the one its state was built for.
/// Those go the old way and answer the same.
///
/// # Errors
///
/// If a total does not fit the declared return type, which is the error [`Accumulator::finish`]
/// raises for the same total, and an internal error if a slot is outside the states.
pub fn finish_run(
    states: &[Accumulator],
    at: &[usize],
    stride: usize,
    offset: usize,
    ty: &LogicalType,
) -> Result<Option<Vector>> {
    let Some(first) = at.first() else {
        return Ok(None);
    };
    let reach = |slot: usize| {
        states
            .get(slot * stride + offset)
            .ok_or_else(|| Error::internal(format!("group {slot} has no state for call {offset}")))
    };
    let Some(route) = reach(*first)?.route(ty) else {
        return Ok(None);
    };
    // One `true` per row rather than a bitmap, because the mask is built from the run in one call
    // afterwards and a byte a row is what that call reads. At a chunk of 2048 groups it is 2 KiB.
    let mut valid = vec![true; at.len()];
    let data = match route {
        Route::Whole => {
            let mut answers: Vec<i128> = Vec::with_capacity(at.len());
            for (row, &slot) in at.iter().enumerate() {
                match reach(slot)?.answer() {
                    Some(Answer::Whole(total)) => answers.push(total),
                    Some(Answer::Null) => {
                        valid[row] = false;
                        answers.push(0);
                    }
                    // A run whose first state took a route and whose later states cannot is a state
                    // array that holds two shapes for one call, which the operator does not build.
                    _ => return Ok(None),
                }
            }
            narrow(&answers, &valid, ty)?
        }
        Route::Double | Route::Float => {
            let mut answers: Vec<f64> = Vec::with_capacity(at.len());
            for (row, &slot) in at.iter().enumerate() {
                match reach(slot)?.answer() {
                    Some(Answer::Real(answer)) => answers.push(answer),
                    Some(Answer::Null) => {
                        valid[row] = false;
                        answers.push(0.0);
                    }
                    _ => return Ok(None),
                }
            }
            if route == Route::Float {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a declared FLOAT result is a FLOAT"
                )]
                Data::Float32(answers.iter().map(|&answer| answer as f32).collect())
            } else {
                Data::Float64(answers.into())
            }
        }
    };
    let vector = Vector::flat(ty.clone(), data)?;
    Ok(Some(vector.with_validity(Validity::from_run(&valid))))
}

/// A column of whole answers, one a group, as [`finish_run`] writes one for a total or a count,
/// with a null where `valid` is false.
///
/// For a caller that added its groups up itself rather than keeping an accumulator for each, so
/// that an answer that does not fit the declared type raises the error it would have raised there.
///
/// # Errors
///
/// If an answer does not fit in `ty`, or `ty` is not a whole number.
pub fn whole_answers(answers: &[i128], valid: &[bool], ty: &LogicalType) -> Result<Vector> {
    let data = narrow(answers, valid, ty)?;
    Ok(Vector::flat(ty.clone(), data)?.with_validity(Validity::from_run(valid)))
}

/// A run of whole answers as the layout `ty` stores, which is the check [`fit`] makes per value.
///
/// A null row carries a zero in the run and is skipped by the range check, because a value that is
/// not there cannot fail to fit and the mask is what says it is not there.
fn narrow(answers: &[i128], valid: &[bool], ty: &LogicalType) -> Result<Data> {
    macro_rules! narrowed {
        ($variant:ident, $native:ty) => {{
            let mut out: Vec<$native> = Vec::with_capacity(answers.len());
            for (row, &total) in answers.iter().enumerate() {
                if !valid[row] {
                    out.push(0);
                    continue;
                }
                out.push(<$native>::try_from(total).map_err(|_| {
                    Error::out_of_range(format!("a sum of {total} does not fit in {ty}"))
                })?);
            }
            Data::$variant(out.into())
        }};
    }
    Ok(match ty.physical() {
        PhysicalType::Int8 => narrowed!(Int8, i8),
        PhysicalType::Int16 => narrowed!(Int16, i16),
        PhysicalType::Int32 => narrowed!(Int32, i32),
        PhysicalType::Int64 => narrowed!(Int64, i64),
        PhysicalType::Int128 => Data::Int128(answers.to_vec().into()),
        PhysicalType::UInt8 => narrowed!(UInt8, u8),
        PhysicalType::UInt16 => narrowed!(UInt16, u16),
        PhysicalType::UInt32 => narrowed!(UInt32, u32),
        PhysicalType::UInt64 => narrowed!(UInt64, u64),
        PhysicalType::UInt128 => narrowed!(UInt128, u128),
        other => {
            return Err(Error::internal(format!(
                "a whole aggregate answer cannot be written as {other:?}"
            )));
        }
    })
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
    update_tallied(states, slots, None, stride, offset, input, rows)
}

/// How many of a chunk's rows land in each group, or none if there are too many groups for the
/// locals of `few` to be the way the chunk is folded.
///
/// Every sum, mean and count over a few groups used to count its rows again as it added them, which
/// on q01 is eight counts a row that all come out the same whenever the argument has no nulls. This
/// is the count taken once for the chunk, for [`update_tallied`] to hand to each of them.
#[must_use]
pub fn group_tally(slots: &[usize], groups: usize) -> Option<Vec<i64>> {
    if groups > FEW || groups.saturating_mul(4) > slots.len() {
        return None;
    }
    let mut counts = vec![0_i64; groups * LANES];
    for (row, &slot) in slots.iter().enumerate() {
        if slot == NOWHERE {
            continue;
        }
        *counts.get_mut(slot.wrapping_mul(LANES) | (row % LANES))? += 1;
    }
    Some(counts.chunks_exact(LANES).map(|lanes| lanes.iter().sum()).collect())
}

/// [`update_scattered`] with the chunk's [`group_tally`], which it takes as the count of every group
/// whenever the argument has no nulls rather than counting the rows again.
///
/// # Errors
///
/// The errors [`update_scattered`] raises.
pub fn update_tallied(
    states: &mut [Accumulator],
    slots: &[usize],
    tally: Option<&[i64]>,
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
    let kind = first.kind();
    // Which way a min or a max runs, taken before the loops below borrow the states they update.
    let extreme = match first.state {
        State::Extreme { least, .. } => Some(least),
        _ => None,
    };
    // Cut to the rows there are, so that the loops below index it without a check of their own.
    // The length was compared against `rows` just above, which is the one place it has to be.
    let into = Where { slots: &slots[..rows], stride, offset, tally };
    // `count(*)` reads nothing, so it never asks for the argument it does not have.
    if kind == Kind::CountStar {
        if few(states, into, rows, Live::All, Feed::Counted, |_| 0)? {
            return Ok(());
        }
        for row in 0..rows {
            let Some(index) = into.index(row) else { continue };
            if let State::Counted { count, .. } = &mut states[index].state {
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
    if let Some(least) = extreme
        && ranked_extremes(states, into, input, rows, &nulls, least)?
    {
        return Ok(());
    }
    if extreme.is_some()
        && input.logical_type() == &LogicalType::Varchar
        && matches!(input.form(), Form::Flat | Form::Dictionary | Form::StringView | Form::Rle)
    {
        // Bytes rather than text, because the comparison below is a comparison of bytes and asking
        // for a `&str` would check that every row of the column is UTF-8 on the way past. That check
        // is the whole column read again: on `SELECT MIN(Referer) ... GROUP BY` over the million row
        // ClickBench sample it was 210 million instructions, a tenth of the query, to produce a
        // `&str` that was turned straight back into the bytes it came from. It is paid where it
        // means something instead, which is the row that becomes a group's answer.
        //
        // row at a time: each input belongs to one group; only a new extreme owns its text.
        for row in 0..rows {
            if !nulls.is_valid(row) {
                continue;
            }
            let Some(index) = into.index(row) else { continue };
            let bytes = input.try_bytes_at(row)?.ok_or_else(|| {
                Error::internal("a valid varchar row had no borrowed text".to_string())
            })?;
            let State::Extreme { held, least } = &mut states[index].state else {
                return Err(Error::internal("a string extreme into another state".to_string()));
            };
            match held {
                Some(current) => {
                    let Value::Varchar(previous) = current.settle()? else {
                        return Err(Error::internal(
                            "a varchar extreme held another type".to_string(),
                        ));
                    };
                    let better = if *least {
                        bytes < previous.as_bytes()
                    } else {
                        bytes > previous.as_bytes()
                    };
                    if better {
                        // Written over the string the group already holds rather than into a new
                        // one, because a group is made once and its answer is replaced many times.
                        previous.clear();
                        previous.push_str(utf8(bytes)?);
                    }
                }
                None => {
                    let text = Value::Varchar(utf8(bytes)?.to_owned());
                    *held = Some(Extremum::Held(Box::new(text)));
                }
            }
        }
        return Ok(());
    }
    let Some(first) = states.get(offset) else {
        return Err(Error::internal(format!(
            "an aggregate at {offset} of {} accumulators",
            states.len()
        )));
    };
    let feed = feed_of(first, input.logical_type());
    if let Some(feed) = feed
        && spread(states, into, input, rows, nulls.live(), feed)?
    {
        return Ok(());
    }
    // An aggregate reads one vector, so its form goes in both halves of the report.
    fallback::record(Kernel::Aggregate, input.form(), input.form());
    // row at a time: the path recorded on the line above, which exists to be correct for a column
    // `spread` does not cover and counts itself so that column shows up.
    for row in 0..rows {
        let Some(index) = into.index(row) else { continue };
        let value = input.try_value_at(row)?;
        states[index].update(std::slice::from_ref(&value))?;
    }
    Ok(())
}

/// Folds every argument of one call into many accumulators a row at a time, for the aggregates that
/// are not a count, a total or an extreme, or says `false` for those and touches nothing.
///
/// The layout is the one [`update_scattered`] folds into, and the caller asks this first. These
/// states take every argument of the call rather than the first one, and some of them keep a null
/// where every aggregate the other paths serve skips it, so none of those paths can stand in.
///
/// # Errors
///
/// What [`Accumulator::update`] raises, and an internal error if the slots or the vectors are
/// shorter than the rows.
pub fn update_general(
    states: &mut [Accumulator],
    slots: &[usize],
    stride: usize,
    offset: usize,
    inputs: &[Vector],
    rows: usize,
) -> Result<bool> {
    if !matches!(states.get(offset), Some(Accumulator { state: State::General(_) })) {
        return Ok(false);
    }
    if slots.len() < rows || inputs.iter().any(|input| input.len() < rows) {
        return Err(Error::internal(format!("an aggregate handed {rows} rows and less to fold")));
    }
    let into = Where { slots: &slots[..rows], stride, offset, tally: None };
    let column = match (&states[offset].state, inputs.first()) {
        (State::General(general), Some(input)) if general.takes_columns() => {
            Column::of(input, rows).map(|column| (column, input.validity()))
        }
        _ => None,
    };
    if let Some((column, valid)) = column {
        for row in (0..rows).filter(|&row| valid.is_valid(row)) {
            let Some(index) = into.index(row) else { continue };
            let Some(Accumulator { state: State::General(general) }) = states.get_mut(index) else {
                return Err(Error::internal(format!("an aggregate state at {index} is not held")));
            };
            general.push_column(column, row, inputs)?;
        }
        return Ok(true);
    }
    let keys = match &states[offset].state {
        State::General(general) if general.keyed() => by_column(inputs, rows),
        _ => None,
    };
    let mut args = Vec::with_capacity(inputs.len());
    for row in 0..rows {
        let Some(index) = into.index(row) else { continue };
        if let Some((column, valid)) = keys
            && valid.is_valid(row)
            && let Some(Accumulator { state: State::General(general) }) = states.get(index)
            && general.cannot_take(Key::at(column, row), inputs.len())
        {
            continue;
        }
        args.clear();
        for input in inputs {
            args.push(input.try_value_at(row)?);
        }
        let Some(state) = states.get_mut(index) else {
            return Err(Error::internal(format!("an aggregate state at {index} is out of range")));
        };
        state.update(&args)?;
    }
    Ok(true)
}

/// The second argument of an `arg_min` or `arg_max` call read as a typed column, with its validity,
/// or `None` when it is not one a [`Column`] reads.
fn by_column(args: &[Vector], rows: usize) -> Option<(Column<'_>, &Validity)> {
    let by = args.get(1)?;
    Column::of(by, rows)
        .filter(|column| !matches!(column, Column::Flags(_)))
        .map(|column| (column, by.validity()))
}

/// Folds one vector into many accumulators a run of rows at a time, where every row of a run
/// belongs to one group.
///
/// `runs` is the chunk cut into runs of one slot, each given as its slot and the row it ends
/// before, so the first starts at row zero and each starts where the last ended. A chunk whose key
/// the rows are sorted on comes in a handful of runs, and folding each run as a whole is one
/// accumulator touched per run instead of one per row: a count is a length, and a total is a sum
/// over a slice the compiler can keep in registers. That is how ClickBench 28 reads its
/// `AVG(STRLEN(URL))` and its `COUNT(*)` over rows sorted on `CounterID`.
///
/// `false`, with nothing touched, for what this does not cover, which is anything but a count, an
/// exact total or an exact mean, and any of those over a column with a null in it. The caller then
/// goes the way [`update_scattered`] goes. A float total is left out on purpose, because adding a
/// run up on its own first would round differently from adding its rows one at a time into what the
/// group already holds.
///
/// A flat column is read where it lies. A dictionary, which is the form every stored decimal of
/// TPC-H arrives in, is read into a run of `i64` first by `coded_runs` and then folded exactly as a
/// flat column is. A count needs no values at all, so it covers every form there is.
///
/// A mean that has already gone inexact takes its run a row at a time for the same reason. An
/// exact total is the same answer in any order, and the only thing adding a run as one number can
/// change is at which row an `i128` overflows, which a column of 64 bit values cannot reach.
///
/// # Errors
///
/// The overflow an exact total raises, as [`update_scattered`] raises it, and an internal error if
/// the runs do not cover exactly the rows given or if the vector is shorter than them.
pub fn update_runs(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    stride: usize,
    offset: usize,
    input: Option<&Vector>,
    rows: usize,
) -> Result<bool> {
    if runs.last().map_or(0, |&(_, end)| end) != rows {
        return Err(Error::internal(format!("runs that do not end at the {rows} rows given")));
    }
    let Some(first) = states.get(offset) else {
        return Ok(false);
    };
    if first.kind() == Kind::General {
        return Ok(false);
    }
    let group = |slot: usize| (slot != NOWHERE).then(|| slot * stride + offset);
    if first.kind() == Kind::CountStar {
        count_runs(states, runs, group);
        return Ok(true);
    }
    let Some(input) = input else { return Ok(false) };
    if input.len() < rows || !all_valid(input) {
        return Ok(false);
    }
    let Some(feed) = feed_of(first, input.logical_type()) else {
        return Ok(false);
    };
    // A count of a column with no nulls in it is the length of each run whatever the column holds,
    // and in whatever form it holds it, since not one value is read to answer it.
    if matches!(feed, Feed::Counted) {
        count_runs(states, runs, group);
        return Ok(true);
    }
    match feed {
        Feed::Total | Feed::Whole { .. } => {}
        Feed::Counted | Feed::Real { .. } | Feed::Extreme(_) => return Ok(false),
    }
    if input.form() != Form::Flat {
        let Some(values) = coded_runs(input, rows) else { return Ok(false) };
        one_runs(states, runs, stride, offset, &values, feed)?;
        return Ok(true);
    }
    let Some(data) = input.data().filter(|data| data.len() >= rows) else {
        return Ok(false);
    };
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    one_runs(states, runs, stride, offset, &values.as_slice()[..rows], feed)?;
                })+
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(exact, each);
    Ok(true)
}

/// Folds one call over a run of values, by [`few_runs`] where that fits and a run at a time where it
/// does not.
///
/// This is [`update_runs`]'s body with the values already in hand, so that a flat column and a
/// dictionary read into a run of `i64` reach the same loop rather than two copies of it.
///
/// # Errors
///
/// The overflow an exact total raises, and an internal error for a run folded into a state whose feed
/// does not fit, which is a bug in the caller.
fn one_runs<T: Copy + TryInto<i64> + Into<i128>>(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    stride: usize,
    offset: usize,
    values: &[T],
    feed: Feed,
) -> Result<()> {
    if few_runs(states, runs, stride, offset, values, feed)? {
        return Ok(());
    }
    let mut start = 0;
    for &(slot, end) in runs {
        let run = &values[start..end];
        let length = end - start;
        start = end;
        let Some(index) = (slot != NOWHERE).then(|| slot * stride + offset) else { continue };
        match (&mut states[index].state, feed) {
            (State::Whole { total, seen, .. } | State::Scaled { total, seen, .. }, Feed::Total) => {
                let sum = run_total(run).ok_or_else(overflowed)?;
                *total = total.checked_add(sum).ok_or_else(overflowed)?;
                *seen = true;
            }
            (State::Mean { total, seen, exact, scale: held, .. }, Feed::Whole { scale }) => {
                *held = scale;
                *seen += length as i64;
                let sum = run_total(run).filter(|_| *exact);
                match sum.and_then(|sum| total.checked_add(sum)) {
                    Some(sum) => *total = sum,
                    None => {
                        for &value in run {
                            let number = value.into();
                            match total.checked_add(number).filter(|_| *exact) {
                                Some(sum) => *total = sum,
                                None => widened(total, exact, number),
                            }
                        }
                    }
                }
            }
            _ => {
                return Err(Error::internal(
                    "a run into a state its feed does not fit".to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Whether every row of a vector is valid, asked without building the mask [`nulls_of`] would build.
///
/// A column that points somewhere else keeps its nulls inside the values it points at, and reading
/// those through the codes is a gather and an allocation. Both run paths only want to know whether
/// there are any at all, and a column with none says so from the validities alone. One with any goes
/// the row at a time way regardless, and that way asks its own question.
///
/// The arms are [`nulls_of`]'s arms, so that a form this says nothing about is a form that keeps its
/// nulls where the vector's own validity is. A column of empty codes over all invalid values is the
/// one case this is stricter about than [`nulls_of`], and being stricter only sends a chunk of no rows
/// down a slower path.
fn all_valid(input: &Vector) -> bool {
    matches!(input.validity(), Validity::AllValid)
        && match input.dictionary_parts().or_else(|| input.run_parts()) {
            Some((_, values)) => all_valid(values),
            None => true,
        }
}

/// The numbers of a chunk of a column that is not laid out flat, laid out flat for the run loops.
///
/// A stored decimal column of a TPC-H table arrives packed, or as a dictionary over a packed run,
/// which is what a column of few distinct values is written as, and the run loops ask for a flat
/// column. So q01's `l_quantity`, `l_extendedprice` and `l_discount` were folded a row at a time
/// through [`spread`] while the two decimals its own arithmetic computed, which are flat because
/// nothing stored them, went down the run path beside them.
///
/// Reading the chunk out first costs a pass and an allocation and buys the run loops back, which on
/// a mean run of 2.81 rows is three calls off the row at a time path and onto one walk they share.
/// The codes are read a block at a time, by [`rudb_vector::Packed::unpack`] where the rows are the
/// packed run in order and by [`rudb_vector::Packed::values_at`] where a dictionary points into it.
///
/// `None` for anything the run loops could not have taken anyway: a form that is neither packed nor
/// pointing at something, a payload that is neither a packed run nor a run of exact numbers, and any
/// value that does not fit an `i64`, which is where the locals of both fold loops keep their totals.
/// What zero means in a packed run, as an `i64`, or `None` if the run reaches past one.
///
/// Asked of the two ends of the packed range rather than of each value, since a code is at most the
/// ceiling and at least the base, so a run whose ends both fit has no value in it that does not.
fn packed_base(packed: &rudb_vector::Packed<'_>) -> Option<i64> {
    let base = i64::try_from(packed.base()).ok()?;
    i64::try_from(packed.ceiling()).ok().map(|_| base)
}

fn coded_runs(input: &Vector, rows: usize) -> Option<Vec<i64>> {
    if let Some(packed) = input.packed_parts() {
        let base = packed_base(&packed)?;
        // One vector and one pass. Unpacking the codes and adding the base to them separately wanted a
        // vector of codes zeroed before it was written and a second walk of every row, which showed up
        // as a per chunk `memset` under this function in the q01 profile.
        let mut out = Vec::new();
        packed.unpack_mapped(0, rows, &mut out, |code| base.wrapping_add(code as i64));
        return Some(out);
    }
    let (codes, values) = input.positions()?;
    let codes = codes.get(..rows)?;
    if let Some(packed) = values.packed_parts() {
        let base = packed_base(&packed)?;
        return Some(packed.values_at(codes, |code| base.wrapping_add(code as i64)));
    }
    let data = values.data()?;
    macro_rules! read {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => {
                    let values = values.as_slice();
                    let mut out = Vec::with_capacity(codes.len());
                    for &code in codes {
                        out.push((*values.get(code as usize)?).try_into().ok()?);
                    }
                    Some(out)
                })+
                _ => None,
            }
        };
    }
    rudb_vector::for_each_layout!(exact, read)
}

/// Folds several calls of one chunk in one pass over its runs, rather than one pass each.
///
/// [`update_runs`] is asked about one call, so a chunk with five calls on the run path walks the same
/// run list five times. What a run costs before any value of it is read, counted out of the
/// disassembly on server2, is twenty one instructions: the run's end loaded, the slice of values it
/// covers bounds checked, the slot read and checked against nothing and against the group count, the
/// local for that slot indexed and loaded, the total stored back and the run pointer advanced. A value
/// inside the run costs four. TPC-H q01's mean run of equal `(l_returnflag, l_linestatus)` is 2.81
/// rows, so the twenty one is paid five times over 2.13 million runs to read eleven values each time,
/// and eleven of the query's instructions per row are the same walk done over again.
///
/// So this reads each run once and folds every call that can share it inside, which leaves the walk,
/// the slot and the row count paid once for the chunk and per call only a load, the adds and a store.
/// The row count is one number for all of them because they all see the same runs, which is the other
/// half of the saving: it was counted per call before.
///
/// `wanted` is the calls the caller is offering, as a bit per offset into a group's accumulators, and
/// the answer is the calls this took. A call it did not take was not touched at all, so the caller
/// folds that one the way it folded it before. Offsets past the width of the mask are never taken,
/// which is nothing a plan reaches: sixty four folding aggregates in one `GROUP BY` is far past the
/// point where finding runs pays for itself.
///
/// Only calls that read values of one layout share a pass, because the value loop is written once per
/// layout and a loop that asked which layout it was reading, per call and per run, would spend more on
/// the question than the sharing saves. The pass takes the layout the most of the offered calls have,
/// and a caller with two layouts in its chunk asks again with what is left.
///
/// A column that points somewhere else is read out into a run of `i64` by `coded_runs` first, and a
/// flat column of `i64` joins that pass, since a read out column and a flat one read the same way once
/// the reading is done. The reading costs a pass and an allocation, so it happens only for the calls
/// whose pass is the one being taken, and a chunk whose flat calls of another layout outnumber them
/// pays nothing for it. q01 is that chunk the other way about: three calls over stored decimals, which
/// arrive packed, and two over decimals its own arithmetic computed, which are flat and `i64` because
/// `DECIMAL(18, 4)` and `DECIMAL(18, 6)` both fit one, so all five walk the runs together.
///
/// A count reads no value, so it goes on whichever pass is first and asks nothing of the layout. What a
/// count wants out of a run is its length, and the walk is adding those up per group regardless, so a
/// count costs the pass one add per group at the end and nothing at all per run. `count_runs` is what
/// it would otherwise be, and that is a walk of its own for one add a run.
///
/// # Errors
///
/// The overflow a total raises, as [`update_runs`] raises it, and an internal error if the runs do not
/// cover exactly the rows given.
pub fn update_shared_runs(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    stride: usize,
    inputs: &[Option<&Vector>],
    wanted: u64,
    rows: usize,
) -> Result<u64> {
    if wanted.count_ones() < 2 {
        return Ok(0);
    }
    if runs.last().map_or(0, |&(_, end)| end) != rows {
        return Err(Error::internal(format!("runs that do not end at the {rows} rows given")));
    }
    let groups = states.len().checked_div(stride).unwrap_or(usize::MAX);
    if groups > FEW || groups.saturating_mul(4) > rows {
        return Ok(0);
    }
    let mut ready = Vec::new();
    let mut counting = Vec::new();
    let mut coded = Vec::new();
    for (offset, input) in inputs.iter().enumerate().take(u64::BITS as usize) {
        if wanted >> offset & 1 == 0 {
            continue;
        }
        match shareable(states, stride, offset, *input, rows, groups) {
            Some(Share::Counted) => counting.push(offset),
            Some(Share::Folded(call)) => ready.push(call),
            Some(Share::Coded(offset, feed, input)) => coded.push((offset, feed, input)),
            None => {}
        }
    }
    // The layout the most of them read, since a pass covers one layout and the caller asks again for
    // what is left. Counted the plain way because the list is as long as the query has aggregates.
    let mut picked = None;
    let mut most = 0;
    for call in &ready {
        let same =
            ready.iter().filter(|other| mem::discriminant(other.data) == call.kind()).count();
        if same > most {
            most = same;
            picked = Some(call);
        }
    }
    // One call is not sharing anything. Two are, whichever two they are, because what the second one
    // saves is a walk of the runs and the counts among them save a walk for an add.
    if most.max(coded.len()) + counting.len() < 2 {
        return Ok(0);
    }
    let took = counting.iter().fold(0_u64, |took, &offset| took | 1 << offset);
    // A flat column of `i64` reads exactly as a column read out into a run of `i64` does, so it joins
    // that pass rather than waiting for one of its own. What that is worth is the walk it does not
    // make: the run itself, and not what is read inside it, is most of what a pass costs, so two
    // passes of three columns and two cost half as much again as one pass of five. q01 is that chunk.
    let flat: Vec<(usize, Feed, &[i64])> = ready
        .iter()
        .filter_map(|call| match call.data {
            Data::Int64(values) => Some((call.offset, call.feed, &values.as_slice()[..rows])),
            _ => None,
        })
        .collect();
    // The columns that point somewhere else are read out into runs of `i64` and share a pass with the
    // flat `i64` ones, since what they hold once they are read is one layout whatever they were held
    // in. Read only when the two together outnumber the widest flat layout, so that a chunk whose flat
    // calls are the pass this time pays nothing for the reading, and the caller asks again for what is
    // left.
    let mut read = Vec::with_capacity(coded.len());
    if coded.len() + flat.len() > most {
        for &(offset, feed, input) in &coded {
            if let Some(values) = coded_runs(input, rows) {
                read.push((offset, feed, values));
            }
        }
    }
    if !read.is_empty() && read.len() + flat.len() > most {
        let mut group: Vec<(usize, Feed, &[i64])> =
            read.iter().map(|(offset, feed, values)| (*offset, *feed, values.as_slice())).collect();
        group.extend_from_slice(&flat);
        if group.len() + counting.len() >= 2 {
            if !many_runs(states, runs, stride, &group, &counting, groups)? {
                return Ok(0);
            }
            return Ok(group.iter().fold(took, |took, &(offset, _, _)| took | 1 << offset));
        }
    }
    if most + counting.len() < 2 {
        return Ok(0);
    }
    let Some(picked) = picked else {
        // Nothing but counts, so the pass is the walk and the lengths and there is no value loop to
        // pick a layout for. The width the locals are asked for is zero, so which one this is has no
        // bearing on anything past naming a type to write the loop that never runs.
        return many_runs::<i64>(states, runs, stride, &[], &counting, groups)
            .map(|shared| if shared { took } else { 0 });
    };
    macro_rules! shared {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match picked.data {
                $(Data::$variant(_) => {
                    let mut group = Vec::with_capacity(most);
                    for call in &ready {
                        if let Data::$variant(values) = call.data {
                            group.push((call.offset, call.feed, &values.as_slice()[..rows]));
                        }
                    }
                    if !many_runs(states, runs, stride, &group, &counting, groups)? {
                        return Ok(0);
                    }
                    group.iter().fold(took, |took, &(offset, _, _)| took | 1 << offset)
                })+
                _ => 0,
            }
        };
    }
    Ok(rudb_vector::for_each_layout!(exact, shared))
}

/// What one call brings to a shared pass over the runs.
enum Share<'r> {
    /// A count of the rows, which the walk has the length of each run for already.
    Counted,
    /// A total of the values of a column, which is a loop inside each run.
    Folded(Ready<'r>),
    /// A total of the values of a column that points somewhere else, so the chunk of it the runs cover
    /// is read out flat before any of them is walked.
    Coded(usize, Feed, &'r Vector),
}

/// One value reading call the shared pass has resolved down to what its run walk needs from it.
struct Ready<'r> {
    /// Where this call's accumulator sits inside each group's run of them.
    offset: usize,
    feed: Feed,
    data: &'r Data,
}

impl Ready<'_> {
    /// Which layout the call's column is held in, to match it against another call's.
    fn kind(&self) -> mem::Discriminant<Data> {
        mem::discriminant(self.data)
    }
}

/// What [`update_shared_runs`] needs to know about one call, or none for a call it cannot take.
///
/// These are [`update_runs`]'s own questions in [`update_runs`]'s order, asked of one call without
/// touching anything, so that a call the shared pass turns down is a call the caller can still fold
/// the old way. The one question it does not ask is the layout, which is the caller's to match up.
fn shareable<'r>(
    states: &[Accumulator],
    stride: usize,
    offset: usize,
    input: Option<&'r Vector>,
    rows: usize,
    groups: usize,
) -> Option<Share<'r>> {
    let first = states.get(offset)?;
    if first.kind() == Kind::General {
        return None;
    }
    // A `COUNT(*)` has no argument to ask anything of, which is why it is asked about first here as it
    // is there.
    if first.kind() == Kind::CountStar {
        return Some(Share::Counted);
    }
    let input = input?;
    if input.len() < rows || !all_valid(input) {
        return None;
    }
    let feed = feed_of(first, input.logical_type())?;
    match feed {
        Feed::Total => {}
        // A count of a column with no nulls in it is the length of each run whatever the column holds,
        // so it wants the same thing a `COUNT(*)` wants and the values are never read.
        Feed::Counted => return Some(Share::Counted),
        // A mean that has already gone inexact adds its rows one at a time, for the reason
        // [`few_runs`] gives, so a call that would reach one of those stays off this pass.
        Feed::Whole { .. } => {
            for slot in 0..groups {
                let held = states.get(slot * stride + offset)?;
                if matches!(held.state, State::Mean { exact: false, .. }) {
                    return None;
                }
            }
        }
        Feed::Real { .. } | Feed::Extreme(_) => return None,
    }
    // Said here rather than read here, because reading a chunk out costs a pass and the caller only
    // spends that on the calls whose pass it picks.
    if input.form() != Form::Flat {
        return Some(Share::Coded(offset, feed, input));
    }
    let data = input.data().filter(|data| data.len() >= rows)?;
    Some(Share::Folded(Ready { offset, feed, data }))
}

/// [`few_runs`] for several calls at once, which is [`update_shared_runs`]'s whole point.
///
/// The locals are one array rather than one per call, a run of `width` totals per group followed by
/// the group's row count, so that a run resolves its slot once and the totals it then adds into sit
/// next to each other in one cache line for the whole of the inner loop. The count is last rather
/// than first so that the totals are indexed the way the calls are.
///
/// `counting` is the calls that want the count and no total, so they are nowhere in the loop and read
/// it off the locals at the end beside everything else.
///
/// `false` with nothing written for the same misses [`few_runs`] hands back on, which is a value that
/// does not fit an `i64` and a total that leaves it. Nothing in `states` is written until every run
/// has been read, so the caller can take every one of these calls the slower way and reach the same
/// answer.
///
/// # Errors
///
/// The overflow folding a local into an accumulator raises, and an internal error for a local folded
/// into a state whose feed does not fit, which is a bug here rather than anything a query can cause.
fn many_runs<T: Copy + TryInto<i64>>(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    stride: usize,
    group: &[(usize, Feed, &[T])],
    counting: &[usize],
    groups: usize,
) -> Result<bool> {
    let width = group.len();
    let span = width + 1;
    let Some(cells) = groups.checked_mul(span) else { return Ok(false) };
    let mut folded = vec![0_i64; cells];
    // A width the compiler knows is a walk whose columns and totals are named rather than looked up,
    // which is what the run visit costs most of, so the widths a query reaches each get a walk of
    // their own and anything wider reads its calls out of the list as it goes.
    macro_rules! widths {
        ($($width:literal),+ $(,)?) => {
            match width {
                $($width => walk_runs::<$width, T>(&mut folded, runs, group),)+
                _ => walk_any(&mut folded, runs, span, group),
            }
        };
    }
    if !widths!(1, 2, 3, 4, 5, 6, 7, 8) {
        return Ok(false);
    }
    for (slot, cells) in folded.chunks_exact(span).enumerate() {
        let Some(&count) = cells.last() else { return Ok(false) };
        if count == 0 {
            continue;
        }
        for (&number, &(offset, feed, _)) in cells.iter().zip(group) {
            fold_local(states, slot * stride + offset, feed, number, count)?;
        }
        for &offset in counting {
            fold_local(states, slot * stride + offset, Feed::Counted, 0, count)?;
        }
    }
    Ok(true)
}

/// [`many_runs`]'s walk of the runs, for a pass whose width the compiler knows.
///
/// A visit to a run costs about seventy instructions beyond the values it reads, and q01's runs of one
/// group average 2.81 rows, so what the walk spends is mostly the visiting. Most of that per call: the
/// call's column loaded out of the list, the slice of it the run covers bounds checked, and the group's
/// total loaded and stored back. Named at compile time the columns are registers held across the whole
/// walk, the totals are registers held across the run, and the loop over the calls is not a loop.
///
/// The row loop is outside the call loop rather than inside it, which is what makes the second of those
/// true. Per call per run there is a loop to set up, a counter to step and a bound to test, and at 2.81
/// rows a run that is three instructions of loop for every useful add. The other way round the counter
/// is stepped once for all `W` calls and the adds between two steps of it are straight line code, so
/// q01's five totals cost thirteen instructions a row where they cost twenty five.
///
/// `false` with `folded` left however far it got, which the caller only reads on `true`, for the misses
/// [`many_runs`] documents: a value that does not fit an `i64`, a total that leaves one, and a slot or a
/// run past what the caller said there would be.
fn walk_runs<const W: usize, T: Copy + TryInto<i64>>(
    folded: &mut [i64],
    runs: &[(usize, usize)],
    group: &[(usize, Feed, &[T])],
) -> bool {
    let span = W + 1;
    let Ok(group): std::result::Result<&[(usize, Feed, &[T]); W], _> = group.try_into() else {
        return false;
    };
    let columns: [&[T]; W] = array::from_fn(|call| group[call].2);
    let mut start = 0;
    for &(slot, end) in runs {
        let from = start;
        start = end;
        if slot == NOWHERE {
            continue;
        }
        // Ends that walk backwards are the caller's bug, and taken here rather than left to the
        // subtraction below so that the row loop's bound and every slice it reads are the one length.
        let Some(length) = end.checked_sub(from) else { return false };
        // A slot past the groups is a bug elsewhere, and nothing has been folded yet, so the caller's
        // loop gets to say so.
        let Some(cells) = folded.get_mut(slot * span..).and_then(|rest| rest.get_mut(..span))
        else {
            return false;
        };
        // The one bounds check a call pays per run, so that the row loop reads a slice it already knows
        // the length of. `from_fn` cannot fail, so a column too short for this run is carried out.
        let mut short = false;
        let run: [&[T]; W] = array::from_fn(|call| match columns[call].get(from..end) {
            Some(run) => run,
            None => {
                short = true;
                &[]
            }
        });
        if short {
            return false;
        }
        let mut sums: [i64; W] = array::from_fn(|call| cells[call]);
        for at in 0..length {
            for (sum, values) in sums.iter_mut().zip(run) {
                let Some(&value) = values.get(at) else { return false };
                let Ok(value) = value.try_into() else { return false };
                let Some(next) = sum.checked_add(value) else { return false };
                *sum = next;
            }
        }
        for (cell, &sum) in cells.iter_mut().zip(&sums) {
            *cell = sum;
        }
        cells[W] += length as i64;
    }
    true
}

/// [`walk_runs`] for a pass wider than any width it is written for, reading its calls out of the list.
///
/// A query with nine folding aggregates over one layout in one `GROUP BY` is past the point where the
/// named widths are worth compiling, and a pass of no calls at all comes here too, since a count wants
/// the walk and the run lengths and has no column to read.
fn walk_any<T: Copy + TryInto<i64>>(
    folded: &mut [i64],
    runs: &[(usize, usize)],
    span: usize,
    group: &[(usize, Feed, &[T])],
) -> bool {
    let width = group.len();
    let mut start = 0;
    for &(slot, end) in runs {
        let from = start;
        start = end;
        if slot == NOWHERE {
            continue;
        }
        let Some(cells) = folded.get_mut(slot * span..).and_then(|rest| rest.get_mut(..span))
        else {
            return false;
        };
        for (total, &(_, _, values)) in cells.iter_mut().zip(group) {
            let Some(run) = values.get(from..end) else { return false };
            let mut sum = *total;
            for &value in run {
                let Ok(value) = value.try_into() else { return false };
                let Some(next) = sum.checked_add(value) else { return false };
                sum = next;
            }
            *total = sum;
        }
        cells[width] += (end - from) as i64;
    }
    true
}

/// Adds one group's total of one call, and how many rows it came from, into that call's accumulator.
///
/// This is the end of a pass that kept its totals in locals, and it runs once per group per call
/// rather than once per run, which is what lets it be the slow careful form while the loop above it is
/// the fast one. `count` is what a mean divides by, what a count of the rows is, and what a total reads
/// only as whether it saw a row at all.
///
/// # Errors
///
/// The overflow the add raises, and an internal error for a state whose feed does not fit or an offset
/// past the accumulators, both of which are bugs in the caller.
fn fold_local(
    states: &mut [Accumulator],
    index: usize,
    feed: Feed,
    number: i64,
    count: i64,
) -> Result<()> {
    let number = i128::from(number);
    let Some(held) = states.get_mut(index) else {
        return Err(Error::internal(format!("an aggregate state at {index} is out of range")));
    };
    match (&mut held.state, feed) {
        (State::Counted { count: held, .. }, Feed::Counted) => *held += count,
        (State::Whole { total, seen, .. } | State::Scaled { total, seen, .. }, Feed::Total) => {
            *total = total.checked_add(number).ok_or_else(overflowed)?;
            *seen = true;
        }
        (State::Mean { total, seen, exact, scale: held, .. }, Feed::Whole { scale }) => {
            *held = scale;
            match total.checked_add(number).filter(|_| *exact) {
                Some(sum) => *total = sum,
                None => widened(total, exact, number),
            }
            *seen += count;
        }
        _ => return Err(Error::internal("a run total into another state".to_string())),
    }
    Ok(())
}

/// Adds the length of each run to the count of the group it belongs to.
fn count_runs(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    group: impl Fn(usize) -> Option<usize>,
) {
    let mut start = 0;
    for &(slot, end) in runs {
        if let Some(index) = group(slot)
            && let State::Counted { count, .. } = &mut states[index].state
        {
            *count += (end - start) as i64;
        }
        start = end;
    }
}

/// The exact total of a run of integers, or `None` when it does not fit an `i128`.
///
/// Only a run of 128 bit values can miss, since a run of anything narrower would need more rows
/// than there are to leave the range, and so only that width pays for a check per value.
///
/// A value of 64 bits or fewer is added as its low 32 bits and the rest, into two sums of 64 bits
/// that a block of 2^30 values cannot overflow. Added as an `i128` each, every add waited on the
/// carry out of the one before it, and on ClickBench 28 that chain was four fifths of the fold of
/// `AVG(length(URL))` by `CounterID`. Two sums with no carry between them are two adds a value
/// that do not wait on each other, and a loop the compiler can put in vector registers.
fn run_total<T: Copy + Into<i128>>(run: &[T]) -> Option<i128> {
    if size_of::<T>() > size_of::<u64>() {
        return run.iter().try_fold(0_i128, |sum, &value| sum.checked_add(value.into()));
    }
    let mut total = 0_i128;
    for block in run.chunks(1 << 30) {
        let (mut low, mut high) = (0_u64, 0_i64);
        for &value in block {
            let value: i128 = value.into();
            low += value as u64 & u64::from(u32::MAX);
            high += (value >> 32) as i64;
        }
        total += (i128::from(high) << 32) + i128::from(low);
    }
    Some(total)
}

/// A grouped min or max over a dictionary that sorted its values when it was written.
///
/// The win is that no string is read. Each row turns into the rank of its code, which is one load
/// out of a map four bytes wide per distinct value, and a group keeps the rank it has rather than
/// the text, so a column whose payload is sixty six megabytes over four hundred thousand values is
/// never touched until the groups are finished. Against the byte comparison below it, on ClickBench
/// query 28 over the million row file, this is the difference between a fetch out of a dictionary
/// block per row for eight hundred thousand rows and one fetch per group for ninety five thousand
/// groups.
///
/// `false` when the input is not a dictionary, or is one that does not know its order, and the
/// caller then takes whichever of the slower paths fits. Nothing here decides an answer differently
/// from those, only more cheaply: a rank order is the byte order of the values by the promise
/// [`rudb_vector::TextSource::ranks`] makes.
fn ranked_extremes(
    states: &mut [Accumulator],
    into: Where<'_>,
    input: &Vector,
    rows: usize,
    nulls: &Validity,
    least: bool,
) -> Result<bool> {
    let Some((codes, dictionary)) = input.shared_dictionary_parts() else { return Ok(false) };
    let Some(ranks) = dictionary.code_ranks() else { return Ok(false) };
    if codes.len() < rows {
        return Ok(false);
    }
    // row at a time: a scatter is per row by definition, since two adjacent rows are usually two
    // different groups and there is nothing to reduce before it.
    for (row, &code) in codes.iter().enumerate().take(rows) {
        if !nulls.is_valid(row) {
            continue;
        }
        let Some(index) = into.index(row) else { continue };
        // Giving up here leaves the rows already offered in the groups that took them, and that is
        // harmless: offering a row to a min twice reaches the same min as offering it once, so the
        // path the caller falls back to reads the whole vector again and lands in the same place.
        let Some(&rank) = ranks.get(code as usize) else { return Ok(false) };
        let State::Extreme { held, .. } = &mut states[index].state else {
            return Err(Error::internal("a ranked extreme into another state".to_string()));
        };
        match held {
            Some(current) => current.offer(dictionary, code, rank, least)?,
            None => {
                let kept = Extremum::Ranked { dictionary: dictionary.clone(), code, rank };
                *held = Some(kept);
            }
        }
    }
    Ok(true)
}

/// One vector into an ungrouped extreme, out of the sorted order the file wrote beside a dictionary.
///
/// This is [`ranked_extremes`] for the case where there is one state rather than a table of them,
/// and the shape of the loop changes with it: no group to scatter into means the whole vector is
/// reduced to its winning code first and offered once, so a vector costs one integer compare per row
/// and one [`Extremum::offer`] rather than one per row.
///
/// What it saves is not the compares, it is the reads. The path it replaces reduces the vector to a
/// winning row and then reads the string at that row to hold it, and a point read into a dictionary
/// that lives in a file decodes the block its value sits in and keeps it. One of those per vector
/// over a hundred million rows holds most of the payload by the end. Holding the rank instead reads
/// nothing until the aggregate is finished, and then reads one value.
///
/// `false` when the input is not a dictionary, or is one that does not know its order, or has a code
/// with no rank against it, and the caller then takes the byte path as before. Nothing is written
/// into the state until the vector has been walked without declining, so a decline halfway through
/// leaves the state exactly as it found it.
fn ranked_extreme(
    state: &mut State,
    input: &Vector,
    rows: usize,
    nulls: &Validity,
    least: bool,
) -> Result<bool> {
    let Some((codes, dictionary)) = input.shared_dictionary_parts() else { return Ok(false) };
    let Some(ranks) = dictionary.code_ranks() else { return Ok(false) };
    let Some(codes) = codes.get(..rows) else { return Ok(false) };
    let mut winner: Option<(u32, u32)> = None;
    // row at a time: a code is a number out of the data, so which value it lands on is not known
    // for any row until that row has been read.
    for (row, &code) in codes.iter().enumerate() {
        if !nulls.is_valid(row) {
            continue;
        }
        let Some(&rank) = ranks.get(code as usize) else { return Ok(false) };
        if winner.is_none_or(|(_, held)| if least { rank < held } else { rank > held }) {
            winner = Some((code, rank));
        }
    }
    let State::Extreme { held, .. } = state else {
        return Err(Error::internal("a ranked extreme into another state".to_string()));
    };
    if let Some((code, rank)) = winner {
        match held {
            Some(current) => current.offer(dictionary, code, rank, least)?,
            None => {
                let kept = Extremum::Ranked { dictionary: dictionary.clone(), code, rank };
                *held = Some(kept);
            }
        }
    }
    Ok(true)
}

/// Turns the ranks a set of groups is holding into the values they stand for, in one ordered pass.
///
/// A group that won on a rank holds a dictionary code and nothing else, so the string it answers
/// with still has to come out of the payload. Asked for one group at a time that is a point read
/// per group, and a point read into a dictionary that lives in a file decodes the block its value
/// sits in and keeps it, so a query whose winners are spread over the dictionary ends up holding
/// most of the payload decoded in order to answer a few thousand strings. On the ClickBench file the
/// `MIN(Title)` of query 23 kept four hundred megabytes that way.
///
/// So the codes are gathered first, sorted, and read in one sweep that hands over a block at a time
/// and keeps none of it. Every group wanting a value out of a block gets it while that block is in
/// hand, and a block no group wants is never decoded at all.
///
/// The accumulators settled are the ones the caller is about to emit and not every accumulator it
/// holds, because a `HAVING` or a bound on the group count throws most of them away and reading the
/// values for those would be work for nothing. `slots` names them when the caller has made that
/// choice already, and `groups` with no slots means all of them; `stride` is how many aggregates
/// there are per group, the same layout [`update_scattered`] folds into.
pub fn settle_extremes(
    states: &mut [Accumulator],
    slots: Option<&[usize]>,
    groups: usize,
    stride: usize,
) -> Result<()> {
    /// Which dictionary, which code, and which accumulator wants it.
    type Wanted = (usize, u32, usize);

    let mut dictionaries: Vec<Arc<Vector>> = Vec::new();
    let mut wanted: Vec<Wanted> = Vec::new();
    let emitted = slots.map_or(groups, <[usize]>::len);
    for index in 0..emitted {
        let slot = slots.map_or(index, |slots| slots[index]);
        for call in 0..stride {
            let at = slot * stride + call;
            let Some(state) = states.get(at) else {
                return Err(Error::internal("an extreme to settle is out of range".to_string()));
            };
            let State::Extreme { held: Some(held), .. } = &state.state else { continue };
            let Extremum::Ranked { dictionary, code, .. } = held else { continue };
            let which = match dictionaries.iter().position(|kept| Arc::ptr_eq(kept, dictionary)) {
                Some(found) => found,
                None => {
                    dictionaries.push(Arc::clone(dictionary));
                    dictionaries.len() - 1
                }
            };
            wanted.push((which, *code, at));
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    wanted.sort_unstable();
    let mut start = 0;
    while start < wanted.len() {
        let mut end = start;
        while end < wanted.len() && wanted[end].0 == wanted[start].0 {
            end += 1;
        }
        let dictionary = Arc::clone(&dictionaries[wanted[start].0]);
        settle_swept(states, &dictionary, &wanted[start..end])?;
        start = end;
    }
    Ok(())
}

/// The sweep behind [`settle_extremes`], over the codes wanted out of one dictionary in code order.
fn settle_swept(
    states: &mut [Accumulator],
    dictionary: &Vector,
    wanted: &[(usize, u32, usize)],
) -> Result<()> {
    let mut at = 0;
    let mut found: Vec<(usize, Value)> = Vec::new();
    while at < wanted.len() {
        let first = wanted[at].1 as usize;
        let mut cursor = at;
        found.clear();
        dictionary.sweep_text(first, dictionary.len(), &mut |index: usize, text: &[u8]| {
            while cursor < wanted.len() && wanted[cursor].1 as usize == index {
                found.push((wanted[cursor].2, dictionary.value_of(text)));
                cursor += 1;
            }
            Ok(())
        })?;
        if cursor <= at {
            return Err(Error::internal(
                "a dictionary sweep passed the code it began at".to_string(),
            ));
        }
        for (slot, value) in found.drain(..) {
            let Some(state) = states.get_mut(slot) else {
                return Err(Error::internal("an extreme to settle is out of range".to_string()));
            };
            let State::Extreme { held: Some(held), .. } = &mut state.state else { continue };
            *held = Extremum::Held(Box::new(value));
        }
        at = cursor;
    }
    Ok(())
}

/// The text a row holds, checked once for the row that is going to be kept.
///
/// The same message [`Vector::try_text_at`] gives, because it is the same failure and a caller
/// cannot tell which of the two read the column.
fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes)
        .map_err(|error| Error::conversion(format!("invalid UTF-8 in VARCHAR: {error}")))
}

/// Which accumulator a row belongs to.
#[derive(Clone, Copy)]
struct Where<'w> {
    slots: &'w [usize],
    stride: usize,
    offset: usize,
    /// How many of the rows land in each group, when the caller took that once for the chunk.
    tally: Option<&'w [i64]>,
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
    /// An exact number per row into a plain running total, with nothing to do to it on the way.
    ///
    /// This is [`Self::Whole`] where the state is a sum rather than a mean, which is where the
    /// scale is carried for. A sum has no use for it, so splitting the two lets the add be the
    /// whole of the row loop rather than the end of a call that asks what it is adding into.
    Total,
    /// An exact number per row, at the scale the column holds it at.
    ///
    /// The scale is carried rather than applied. A total of the integers a decimal column stores is
    /// the total of the column with the point moved, so the point moves once where the total is
    /// read and not once per row on the way in. Everything but a mean ignores it, because a sum of
    /// a decimal is declared at the column's own scale and so is already the answer.
    Whole { scale: u8 },
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
        (State::Counted { .. }, _) => Some(Feed::Counted),
        (State::General(_), _) => None,
        // A whole total adds the integers of an integer column. The type is asked about rather
        // than taken for granted because a date and a timestamp are stored as integers too, and
        // summing one of those is not a thing you can do: the row at a time path says `summing a
        // DATE` and declines, so a loop that quietly added the days up would be answering a
        // question the engine has already refused. Anything this arm turns down goes down that
        // path and gets the same refusal.
        (State::Whole { .. }, ty) if ty.is_integer() => Some(Feed::Total),
        (State::Whole { .. }, _) => None,
        (State::Scaled { scale, .. }, LogicalType::Decimal { scale: held, .. })
            if held == scale =>
        {
            Some(Feed::Total)
        }
        (State::Scaled { .. }, _) => None,
        (State::Mean { .. }, ty) if ty.is_integer() => Some(Feed::Whole { scale: 0 }),
        // A mean over a decimal adds the integers the column stores and moves the point once, at
        // the finish. See [`State::Mean`], which is where the scale ends up.
        (State::Mean { .. }, LogicalType::Decimal { scale, .. }) => {
            Some(Feed::Whole { scale: *scale })
        }
        // A mean whose column is a float carries its total in that column's own units, which is
        // why the scale here is the column's and is zero for everything that reaches this arm.
        (State::Mean { .. } | State::Real { .. }, ty) if addable(ty) => {
            Some(Feed::Real { scale: decimal_scale(ty) })
        }
        (State::Mean { .. } | State::Real { .. }, _) => None,
        // A number is compared as a number and anything else is compared the way the comparison
        // kernel says, which a run of `i128` cannot do for a float, a string or a date. A decimal
        // is here too because every row of one column holds the same scale, so the unscaled
        // integers order the way the numbers do. That is q02's grouped `min` over `DECIMAL(15, 2)`.
        // A date, a time and a timestamp are here for the same reason a decimal is. Each one is a
        // single signed integer of a single unit and each one orders the way that integer does, so
        // the loop that compares a run of `i128` answers them exactly as it answers an `INTEGER`.
        // They were left out at first and the cost of leaving them out was not small: a grouped
        // `min` over `l_shipdate` came to 776 instructions a row against 90 for the same `min` over
        // an `INTEGER`, because the type fell through to the path that builds a `Value` per row.
        // See [`../../../spec/perf/15-what-a-row-costs.md`].
        (State::Extreme { .. }, ty)
            if ty.is_integer()
                || matches!(
                    ty,
                    LogicalType::Decimal { .. }
                        | LogicalType::Date
                        | LogicalType::Time
                        | LogicalType::Timestamp
                ) =>
        {
            Some(Feed::Extreme(first.kind() == Kind::Min))
        }
        (State::Extreme { .. }, _) => None,
    }
}

/// Runs a body for every row of a chunk that is not null, with the null question settled before
/// the loop starts rather than asked again on every row.
///
/// This is loop unswitching, written out rather than left to the compiler because the compiler did
/// not do it. The loops below all started as `if !nulls.at(row) { continue }`, and `Live::at` is
/// three lines and `#[inline]`, so it reads like nothing. What it came to on q01 was twelve of the
/// forty five instructions the scatter spent per row: the validity's own discriminant read twice, a
/// load of the bitmap's pointer out of the `Vec` behind it, and a bounds check of the word, all of
/// them per row and none of them changing from one row to the next. See
/// [`../../../spec/perf/15-what-a-row-costs.md`].
///
/// The three arms are the three shapes a validity has. Nothing folds when everything is null, so
/// that arm is empty. Nothing is checked when nothing is null, so that arm is the bare loop. The
/// third takes the words once and indexes them, and cuts the row count to the rows the words cover
/// because a row past the end of a bitmap is not there and reads as invalid, which is the one thing
/// [`rudb_vector::Bitmap::get`] does that indexing does not.
macro_rules! live_rows {
    ($nulls:expr, $rows:expr, |$row:ident| $body:block) => {
        match $nulls {
            Live::None => {}
            Live::All => {
                for $row in 0..$rows $body
            }
            Live::Mask(mask) => {
                let words = mask.words();
                let covered = $rows.min(words.len().saturating_mul(u64::BITS as usize));
                for $row in 0..covered {
                    if words[$row / 64] >> ($row % 64) & 1 == 1 $body
                }
            }
        }
    };
}

/// How many groups a call adds up on its own before it touches an accumulator.
///
/// Past this the locals cost more to clear than the rows they save, and a call with that many
/// groups is one where few rows share a group anyway. A call also needs four rows a group, for the
/// same reason.
const FEW: usize = 256;

/// How many locals [`few`] keeps per group.
///
/// With one local per group, a row's add reads the total the row before it wrote whenever the two
/// rows are in the same group, which on a handful of groups is most rows, and the row then waits on
/// the store of the one before it. On q01 that wait was nearly all of the time in the row loop. With
/// four, rows next to each other add into different locals and the adds overlap.
const LANES: usize = 4;

/// One pass that adds each row into a local total for its group, then folds each group's total into
/// its accumulator once, or false if there are too many groups for that to pay.
///
/// The loops below reach the accumulator of every row: the slot times the stride, the enum asked
/// which state it is, and a 128 bit add checked for overflow. On q01, with four groups and seven
/// aggregates, that was most of what the query spent. Here the row costs the slot, one add into a
/// local and one count, and the rest is paid once per group per chunk.
///
/// The value a row adds has to fit in 65 bits, which every caller checks before it calls, and so a
/// local total cannot overflow before there are 2^62 rows in a call. That is why the adds below are
/// not checked. The fold into the accumulator is checked the way the per row add was, and a sum is
/// range checked again at the finish, so an answer that does not fit says so as it did before.
///
/// Only exact totals, means and counts come here. A float total has to add in row order to round
/// the way the row at a time path rounds, and a local total per group would change that order.
fn few<V: Fn(usize) -> i128>(
    states: &mut [Accumulator],
    into: Where<'_>,
    rows: usize,
    nulls: Live<'_>,
    feed: Feed,
    value: V,
) -> Result<bool> {
    let groups = states.len().checked_div(into.stride).unwrap_or(usize::MAX);
    // Clearing the locals is a fixed cost per call, so a call needs rows enough to pay it back. A
    // partitioned table hands its partitions a few dozen rows at a time over a hundred or so groups,
    // and there it measured at nine percent more on TPC-H q15 before this second condition.
    if groups > FEW
        || groups.saturating_mul(4) > rows
        || !matches!(feed, Feed::Counted | Feed::Total | Feed::Whole { .. })
    {
        return Ok(false);
    }
    // With no nulls every row counts, so the chunk's tally is each group's count and the loop is
    // left with the add alone, or with nothing at all for a count.
    let tallied = match (nulls, into.tally) {
        (Live::All, Some(tally)) if tally.len() == groups => Some(tally),
        _ => None,
    };
    // Each group has LANES locals and a row adds into the one its position picks, so rows next to
    // each other never wait on each other's add. See [`LANES`].
    let mut totals = vec![0_i128; groups * LANES];
    let mut counts = vec![0_i64; if tallied.is_some() { 0 } else { groups * LANES }];
    if tallied.is_none() {
        live_rows!(nulls, rows, |row| {
            let slot = into.slots[row];
            if slot == NOWHERE {
                continue;
            }
            let local = slot.wrapping_mul(LANES) | (row % LANES);
            // A slot past the groups is a bug elsewhere, and nothing has been folded yet, so the
            // loops that index the accumulators directly get to say so.
            let (Some(total), Some(count)) = (totals.get_mut(local), counts.get_mut(local)) else {
                return Ok(false);
            };
            *total = total.wrapping_add(value(row));
            *count += 1;
        });
    } else if !matches!(feed, Feed::Counted) {
        for (row, &slot) in into.slots.iter().enumerate().take(rows) {
            if slot == NOWHERE {
                continue;
            }
            let Some(total) = totals.get_mut(slot.wrapping_mul(LANES) | (row % LANES)) else {
                return Ok(false);
            };
            *total = total.wrapping_add(value(row));
        }
    }
    for (slot, number) in totals.chunks_exact(LANES).enumerate() {
        let count: i64 = match tallied {
            Some(tally) => tally[slot],
            None => counts[slot * LANES..(slot + 1) * LANES].iter().sum(),
        };
        if count == 0 {
            continue;
        }
        let number = number.iter().fold(0_i128, |sum, &lane| sum.wrapping_add(lane));
        let index = slot * into.stride + into.offset;
        match (&mut states[index].state, feed) {
            (State::Counted { count: held, .. }, Feed::Counted) => *held += count,
            (State::Whole { total, seen, .. } | State::Scaled { total, seen, .. }, Feed::Total) => {
                *total = total.checked_add(number).ok_or_else(overflowed)?;
                *seen = true;
            }
            (State::Mean { total, seen, exact, scale: held, .. }, Feed::Whole { scale }) => {
                *held = scale;
                match total.checked_add(number).filter(|_| *exact) {
                    Some(sum) => *total = sum,
                    None => widened(total, exact, number),
                }
                *seen += count;
            }
            _ => return Err(Error::internal("a total per group into another state".to_string())),
        }
    }
    Ok(true)
}

/// [`few`] over runs rather than rows, for the shape where a chunk comes in runs and there are few
/// groups to put them in.
///
/// [`update_runs`] reaches the accumulator once per run rather than once per row, which is the right
/// first move and is not enough. Measured on server2 at SF1, TPC-H q01's mean run of equal
/// `(l_returnflag, l_linestatus)` is 2.81 rows, so a fixed cost per run is a cost per three rows,
/// and what `update_runs` pays per run is the slot resolved, the accumulator array indexed, the
/// state enum matched, a call to [`run_total`] and a checked add into an `i128`. That came to more
/// than the row at a time scatter it replaces, which is why lowering the threshold that lets
/// `slot_runs_of` cut a chunk at all made q01 monotonically worse, up to nine percent at two rows a
/// run. See #1633.
///
/// The fact that makes the fixed cost avoidable rather than merely smaller is that q01 has four
/// groups. Two million runs are landing in four accumulators. So this keeps one `i64` local per
/// group, adds each run into the local for its slot, and folds each local into its accumulator once
/// for the whole call. Per run that is a slot, a bounds check and an add, and per row inside a run
/// it is a load and an `i64` add.
///
/// `i64` rather than the `i128` the accumulators hold, because that halves the add and the traffic
/// and because every add here is checked anyway: a local sum that would leave the range hands the
/// whole call back to the loop below instead, which is where the `i128` lives. Handing back is free
/// of consequence because nothing in `states` is written until every run has been read, so a call
/// that gives up halfway leaves the accumulators exactly as it found them and the caller's loop
/// reads the same runs again and reaches the same answer.
///
/// `false` for more groups than [`FEW`], for a call with too few rows to pay for clearing the
/// locals, for anything but an exact total or an exact mean, and for a value that does not fit an
/// `i64`, which is only ever a 128 bit column.
///
/// # Errors
///
/// The overflow the fold raises, as [`few`] raises it, and an internal error for a local total
/// folded into a state whose feed does not fit, which is a bug in the caller rather than anything a
/// query can cause.
fn few_runs<T: Copy + TryInto<i64>>(
    states: &mut [Accumulator],
    runs: &[(usize, usize)],
    stride: usize,
    offset: usize,
    values: &[T],
    feed: Feed,
) -> Result<bool> {
    let groups = states.len().checked_div(stride).unwrap_or(usize::MAX);
    if groups > FEW
        || groups.saturating_mul(4) > values.len()
        || !matches!(feed, Feed::Total | Feed::Whole { .. })
    {
        return Ok(false);
    }
    // A mean that has already gone inexact adds its rows one at a time, because it is a float by
    // then and the order the rows reach it decides how it rounds. Folding a run's total into one
    // would round differently, so a call that would touch one of those hands the whole thing back.
    // The loop below then takes it a row at a time, which is what [`update_runs`] documents. Asked
    // here rather than in the fold, because the fold writes as it goes and giving up there would
    // leave some groups folded and some not.
    if matches!(feed, Feed::Whole { .. }) {
        for slot in 0..groups {
            let Some(held) = states.get(slot * stride + offset) else { return Ok(false) };
            if matches!(held.state, State::Mean { exact: false, .. }) {
                return Ok(false);
            }
        }
    }
    let mut totals = vec![0_i64; groups];
    // Separate from the totals because a mean needs how many rows it saw and a total needs only
    // whether it saw one, and both are per run here rather than per row.
    let mut counts = vec![0_i64; groups];
    let mut start = 0;
    for &(slot, end) in runs {
        let Some(run) = values.get(start..end) else { return Ok(false) };
        start = end;
        if slot == NOWHERE {
            continue;
        }
        // A slot past the groups is a bug elsewhere, and nothing has been folded yet, so the loop
        // below gets to say so.
        let (Some(total), Some(count)) = (totals.get_mut(slot), counts.get_mut(slot)) else {
            return Ok(false);
        };
        let mut sum = *total;
        for &value in run {
            let Ok(value) = value.try_into() else { return Ok(false) };
            let Some(next) = sum.checked_add(value) else { return Ok(false) };
            sum = next;
        }
        *total = sum;
        *count += run.len() as i64;
    }
    for (slot, &number) in totals.iter().enumerate() {
        let count = counts[slot];
        if count == 0 {
            continue;
        }
        fold_local(states, slot * stride + offset, feed, number, count)?;
    }
    Ok(true)
}

/// One pass over a vector, folding each row into the accumulator it belongs to.
fn spread(
    states: &mut [Accumulator],
    into: Where<'_>,
    input: &Vector,
    rows: usize,
    nulls: Live<'_>,
    feed: Feed,
) -> Result<bool> {
    // Both counts are answered by the mask on its own, whatever the form and whatever the type, so
    // they come back before there is any question of which loop to run.
    if matches!(feed, Feed::Counted) {
        if few(states, into, rows, nulls, feed, |_| 0)? {
            return Ok(true);
        }
        live_rows!(nulls, rows, |row| {
            let Some(index) = into.index(row) else { continue };
            if let State::Counted { count, .. } = &mut states[index].state {
                *count += 1;
            }
        });
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
            // Cut to the rows there are, for the reason `update_scattered` cuts the slots: the
            // gather below reads this once per row and the length is the same every time.
            let codes = &codes[..rows];
            let Some(data) = values.data() else {
                // A dictionary whose values are a packed run, which is the form a stored column of
                // numbers with few distinct values in it arrives in, and the form every decimal of
                // a grouped TPC-H query is read out of a native file as.
                let Some(packed) = values.packed_parts() else { return Ok(false) };
                return packed_into(
                    states,
                    into,
                    input,
                    &packed,
                    |index| codes[index] as usize,
                    rows,
                    nulls,
                    feed,
                );
            };
            let run = Run { input, data, rows, nulls };
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather below indexes without a bound of its own.
            scatter(states, into, &run, |index| codes[index] as usize, feed)
        }
        Form::BitPacked => {
            let Some(packed) = input.packed_parts() else { return Ok(false) };
            packed_into(states, into, input, &packed, identity, rows, nulls, feed)
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
    nulls: Live<'r>,
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
        Feed::Total => total_into(states, into, run, at),
        Feed::Whole { scale } => mean_into(states, into, run, at, scale),
        Feed::Real { scale } => real_into(states, into, run, at, scale),
        Feed::Extreme(least) => extreme_into(states, into, run, at, least),
    }
}

/// An exact number per row into the plain running total of the group that row belongs to.
///
/// The state question is out of the row loop here, as it is in [`mean_into`]. Every accumulator a
/// call owns was made by that call, so they are all the same variant, and [`feed_of`] has already
/// read that variant off the first of them. What is left per row is a load, an add and a store.
///
/// A sum of a decimal is declared at the column's own scale, so unlike a mean there is no scale to
/// carry here and nothing to do to the number between reading it and adding it.
fn total_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
) -> Result<bool> {
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => {
                    let values = values.as_slice();
                    if size_of::<$native>() < 16
                        && few(states, into, run.rows, run.nulls, Feed::Total, |row| {
                            i128::from(values[at(row)])
                        })?
                    {
                        return Ok(true);
                    }
                    live_rows!(run.nulls, run.rows, |row| {
                        let Some(index) = into.index(row) else { continue };
                        let (State::Whole { total, seen, .. }
                            | State::Scaled { total, seen, .. }) = &mut states[index].state
                        else {
                            return Err(Error::internal("an exact total into another".to_string()));
                        };
                        *total = total
                            .checked_add(i128::from(values[at(row)]))
                            .ok_or_else(overflowed)?;
                        *seen = true;
                    });
                })+
                // The same width `mean_into` leaves, for the same reason it leaves it.
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(exact, each);
    Ok(true)
}

/// An exact number per row into the running mean of the group that row belongs to.
///
/// A mean is the only state [`feed_of`] answers [`Feed::Whole`] for, so the state is read here once
/// per row and not asked about, the same way [`total_into`] reads a sum. That matters more here
/// than it does there, because a mean is the one exact state that has to carry the column's scale
/// and so is the one that used to reach the shared fold through a call.
///
/// The widths run up to 128 bits because the add below is checked per row and raises the same
/// overflow the row at a time path raises on the same row, so a total that does not fit says so
/// either way. `UInt128` is the one width left out, since a value above `i128::MAX` has no exact
/// accumulator here at all.
fn mean_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    run: &Run<'_>,
    at: M,
    scale: u8,
) -> Result<bool> {
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => {
                    let values = values.as_slice();
                    if size_of::<$native>() < 16
                        && few(states, into, run.rows, run.nulls, Feed::Whole { scale }, |row| {
                            i128::from(values[at(row)])
                        })?
                    {
                        return Ok(true);
                    }
                    live_rows!(run.nulls, run.rows, |row| {
                        let Some(index) = into.index(row) else { continue };
                        let number = i128::from(values[at(row)]);
                        let State::Mean { total, seen, exact, scale: held, .. } =
                            &mut states[index].state
                        else {
                            return Err(Error::internal("a mean into another".to_string()));
                        };
                        *held = scale;
                        match total.checked_add(number).filter(|_| *exact) {
                            Some(sum) => *total = sum,
                            None => widened(total, exact, number),
                        }
                        *seen += 1;
                    });
                })+
                // A value wider than an `i128` can hold has no exact accumulator here, so it goes
                // the row at a time way, which reports what it cannot add.
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(exact, each);
    Ok(true)
}

/// The same three loops over a packed run, either the vector's own or one a dictionary points into.
///
/// A packed vector's value is its base plus its code, so each loop below is the matching flat one
/// with the base added and the type dispatch gone, since a packing has one width for the whole
/// column rather than a layout per variant. What it is for is the grouped half of #1088: q01 reads
/// a decimal column out of a native file as a dictionary over a packed run, and with no path for
/// that shape every row of it built a `Value` on the way into its group.
///
/// `UInt128` is declined here for the reason [`mean_into`] declines it, which is that a value
/// above `i128::MAX` has no exact accumulator here at all. A packing never holds one anyway, since
/// the span a packing is built from is read at the widths that fit an `i128`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a wide integer past 2^53 losing digits is what a double is, and this is the float path"
)]
#[expect(clippy::too_many_arguments, reason = "the flat path's Run plus the packing it replaces")]
fn packed_into<M: Fn(usize) -> usize>(
    states: &mut [Accumulator],
    into: Where<'_>,
    input: &Vector,
    packed: &rudb_vector::Packed<'_>,
    at: M,
    rows: usize,
    nulls: Live<'_>,
    feed: Feed,
) -> Result<bool> {
    let wide = input.logical_type().physical() == PhysicalType::UInt128;
    let scale = decimal_scale(input.logical_type());
    let base = packed.base();
    // Every code the loops below read, unpacked in bulk before any of them runs. See
    // [`rudb_vector::Packed::unpack`] for what a code at a time was costing.
    let codes = packed.codes_at(at, rows);
    let code = |row: usize| codes[row];
    // A base inside 64 bits and a code of at most 64 is a value inside 65, which is small enough for
    // [`few`]'s unchecked local totals. A base past that is a column no packing here has built.
    if !wide
        && i64::try_from(base).is_ok()
        && few(states, into, rows, nulls, feed, |row| base + i128::from(code(row)))?
    {
        return Ok(true);
    }
    match feed {
        Feed::Counted => Ok(true),
        // The same loop `total_into` is, over a packing rather than a run of values, and the same
        // reason for it: this is where q01 adds a decimal column read out of a native file.
        Feed::Total => {
            if wide {
                return Ok(false);
            }
            live_rows!(nulls, rows, |row| {
                let Some(index) = into.index(row) else { continue };
                let (State::Whole { total, seen, .. } | State::Scaled { total, seen, .. }) =
                    &mut states[index].state
                else {
                    return Err(Error::internal("an exact total into another".to_string()));
                };
                *total = total.checked_add(base + i128::from(code(row))).ok_or_else(overflowed)?;
                *seen = true;
            });
            Ok(true)
        }
        // The same loop `mean_into` is, and the same reason for it.
        Feed::Whole { scale } => {
            if wide {
                return Ok(false);
            }
            live_rows!(nulls, rows, |row| {
                let Some(index) = into.index(row) else { continue };
                let number = base + i128::from(code(row));
                let State::Mean { total, seen, exact, scale: held, .. } = &mut states[index].state
                else {
                    return Err(Error::internal("a mean into another".to_string()));
                };
                *held = scale;
                match total.checked_add(number).filter(|_| *exact) {
                    Some(sum) => *total = sum,
                    None => widened(total, exact, number),
                }
                *seen += 1;
            });
            Ok(true)
        }
        Feed::Real { scale } => {
            let factor = pow10(scale) as f64;
            let scaled = scale != 0;
            live_rows!(nulls, rows, |row| {
                let Some(index) = into.index(row) else { continue };
                let number = (base + i128::from(code(row))) as f64;
                fold_real(&mut states[index], if scaled { number / factor } else { number });
            });
            Ok(true)
        }
        Feed::Extreme(least) => {
            if wide {
                return Ok(false);
            }
            live_rows!(nulls, rows, |row| {
                let Some(index) = into.index(row) else { continue };
                let number = base + i128::from(code(row));
                let State::Extreme { held, .. } = &mut states[index].state else {
                    return Err(Error::internal("an extreme into a total".to_string()));
                };
                let replace = match held {
                    None => true,
                    Some(current) => {
                        let current: &Value = current.settle()?;
                        let against = mark(current, scale).ok_or_else(|| not_narrow(current))?;
                        if least { number < against } else { number > against }
                    }
                };
                // The `Value` is built on a win and not per row, exactly as the flat loop builds it.
                if replace {
                    let value = input.try_value_at(row)?;
                    *held = Some(Extremum::Held(Box::new(value)));
                }
            });
            Ok(true)
        }
    }
}

/// A mean's total once it has stopped being exact, which is the way out of [`mean_into`]'s add.
///
/// A total goes inexact when the exact sum overflows an `i128`, and from then on it is a double
/// kept in the bits of one. Out of line and marked cold because a mean of a column whose sum fits
/// is every mean TPC-H takes, so the row loop is laid out for the add that works and this is the
/// branch it does not take.
#[cold]
fn widened(total: &mut i128, exact: &mut bool, number: i128) {
    let real = if *exact { exactly(*total) } else { mean_real(*total) };
    *total = mean_bits(real + exactly(number));
    *exact = false;
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
            let values = $values.as_slice();
            let convert = $convert;
            live_rows!(run.nulls, run.rows, |row| {
                let Some(index) = into.index(row) else { continue };
                let number = convert(values[at(row)]);
                fold_real(&mut states[index], if scaled { number / factor } else { number });
            });
        }};
    }
    rudb_vector::for_each_layout!(integer, each);
    Ok(true)
}

/// One approximate number into one accumulator.
fn fold_real(into: &mut Accumulator, number: f64) {
    match &mut into.state {
        State::Real { total, seen, .. } => {
            *total += number;
            *seen += 1;
        }
        State::Mean { total, seen, exact, .. } => {
            // The first value that is not whole. What was counted exactly so far comes across as
            // one conversion, and the rest of the group is added the way the float path adds.
            let real = if *exact { exactly(*total) } else { mean_real(*total) };
            *total = mean_bits(real + number);
            *exact = false;
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
    let scale = decimal_scale(run.input.logical_type());
    macro_rules! each {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match run.data {
                $(Data::$variant(values) => {
                    let values = values.as_slice();
                    // row at a time: a scatter is per row by definition, since two adjacent rows
                    // are usually two different groups and there is nothing to reduce before it.
                    // The `Value` below is built on a win rather than on a row, which is the part
                    // that makes this loop worth having over the one it replaced.
                    live_rows!(run.nulls, run.rows, |row| {
                        let Some(index) = into.index(row) else { continue };
                        let number = i128::from(values[at(row)]);
                        let State::Extreme { held, .. } = &mut states[index].state else {
                            return Err(Error::internal("an extreme into a total".to_string()));
                        };
                        let replace = match held {
                            None => true,
                            Some(current) => {
                                let current: &Value = current.settle()?;
                                let against =
                                    mark(current, scale).ok_or_else(|| not_narrow(current))?;
                                if least { number < against } else { number > against }
                            }
                        };
                        // The `Value` is built on a win and not per row, which for a column that
                        // arrives sorted is once and for a column that arrives shuffled is about
                        // the harmonic number of the rows in the group.
                        if replace {
                            let value = run.input.try_value_at(row)?;
                            *held = Some(Extremum::Held(Box::new(value)));
                        }
                    });
                })+
                _ => return Ok(false),
            }
        };
    }
    rudb_vector::for_each_layout!(exact, each);
    Ok(true)
}

fn not_narrow(value: &Value) -> Error {
    Error::not_implemented(format!("summing a {}", value.logical_type()))
}

/// The number a held extreme is compared as against the numbers read out of a column.
///
/// A decimal column carries one scale for every row in it, so the unscaled integers of two values
/// out of the same column order the same way the values themselves do, and comparing those is
/// comparing the numbers. The scale is passed in rather than taken off the value so that a held
/// value from somewhere other than this column cannot be compared against it by accident.
fn mark(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } if held == scale => Some(unscaled),
        Value::Decimal { .. } => None,
        // The same three types [`feed_of`] lets through, read as the integer they are stored as.
        // This is the grouping [`rudb_vector::Vector::signed_at`] already makes for the same reason.
        Value::Date(days) => Some(i128::from(days)),
        Value::Time(micros) | Value::Timestamp(micros) => Some(i128::from(micros)),
        _ => integral(value),
    }
}

/// An average from an exact total of `seen` values stored at `scale`, the way `avg` finishes one.
///
/// One division, by the count and the scale together. Where the column was a decimal the total is
/// the sum of the integers it stores, so the point has still to go back, and dividing by the count
/// and then by a hundred rounds twice where dividing by a hundred times the count rounds once. That
/// last digit is what duckdb answers with, and on the eight cells of TPC-H query 1 where the two
/// orders differ the single division is the one that agrees with it. A power of ten is exact as a
/// double well past any scale a decimal can declare, so the product is exact too.
///
/// `__rudb_mean` in the scalar kernels calls this as well, for an `avg` the optimizer has split into
/// a `sum` and a `count`, so that the two ways of reaching an average reach the same bits.
pub(crate) fn divide_mean(total: f64, seen: i64, scale: u8) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "the count of rows in one group is well inside the exact range"
    )]
    let divisor = seen as f64 * pow10(scale) as f64;
    total / divisor
}

/// An exact total as the double a mean divides, which is the one rounding `avg` over whole numbers
/// is allowed to do and is where duckdb does it too.
#[expect(
    clippy::cast_precision_loss,
    reason = "a total past 2^53 rounding once here is the definition of a double result"
)]
pub(crate) fn exactly(total: i128) -> f64 {
    total as f64
}

fn mean_bits(total: f64) -> i128 {
    i128::from(total.to_bits())
}

fn mean_real(bits: i128) -> f64 {
    f64::from_bits(bits as u64)
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

/// Whether a column of this type is one the totalling loops can add up.
///
/// A number is, and nothing else is. This has to be asked rather than assumed because a date and a
/// timestamp are held as integers and a loop that dispatches on how the values are stored will
/// happily add the days of one up. The row at a time path refuses that with `summing a DATE`, so a
/// vector loop that answered it would be disagreeing with the engine rather than going faster than
/// it. A string reaches the same place by a different route and is refused the same way.
fn addable(ty: &LogicalType) -> bool {
    ty.is_integer()
        || matches!(ty, LogicalType::Decimal { .. } | LogicalType::Float | LogicalType::Double)
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
            // `DIRECT` says the mapping is the identity, which the loops below turn into a slice
            // of the first `rows` values. That is the only way the compiler gets to see that an
            // index cannot be out of range, and a bounds check per row was most of what this cost.
            collect::<true, _>(data, identity, rows, nulls, want)
        }
        Form::Dictionary | Form::Rle => {
            let (codes, values) = input.positions()?;
            // Cut to the rows wanted rather than checked against them, so that the loops below can
            // see that a row number is inside the codes and drop the check they would otherwise do
            // on every row. The codes are the half of the read that can say this: which value a
            // code lands on is a number out of the data and nothing knows it is in range until it
            // has been looked at.
            let codes = codes.get(..rows)?;
            let Some(data) = values.data() else {
                // A dictionary over a packed run, which is what a stored column of numbers with
                // few distinct values in it is, and where every decimal of TPC-H arrives.
                if let Some(packed) = values.packed_parts() {
                    return from_packed(&packed, |row| codes[row] as usize, rows, nulls, want);
                }
                // A dictionary whose values sit in a file rather than in a run of memory, which is
                // what a scan of a native column hands over. There is nothing for the loops below
                // to read, but an extreme is decided on the bytes and the bytes can be asked for
                // one code at a time, so that much still works here.
                return match want {
                    Want::Extreme(least) => {
                        extreme_bytes(values, codes, nulls, least).map(Contribution::Extreme)
                    }
                    _ => None,
                };
            };
            if let (Want::Whole, Validity::AllValid) = (want, nulls)
                && let Some(total) = tally(data, codes)
            {
                return Some(Contribution::Whole(total));
            }
            collect::<false, _>(data, |index| codes[index] as usize, rows, nulls, want)
        }
        Form::BitPacked => from_packed(&input.packed_parts()?, identity, rows, nulls, want),
        // A constant folds in as one value repeated and a sequence as an arithmetic series, and
        // both have a closed form that is better than any loop. Neither is what a scan of a column
        // produces, so both wait for the counter to ask for them.
        _ => None,
    }
}

/// A packed run read once, either the vector's own or one a dictionary points into.
///
/// A packed vector's value is its base plus its code, so the three loops add the base and carry on
/// exactly as the flat ones do. An extreme does not add anything at all: a code is a monotone
/// function of the value it stands for, so the largest code is the largest value and the base only
/// matters when the winning row is read at the end.
///
/// The mapping is a generic parameter for the reason the rest of this file gives, and it is what
/// lets a dictionary over a packed run come here with its codes rather than through the row at a
/// time path. That is the form every decimal column of a stored TPC-H table arrives in.
fn from_packed<M: Fn(usize) -> usize>(
    packed: &rudb_vector::Packed<'_>,
    at: M,
    rows: usize,
    nulls: &Validity,
    want: Want,
) -> Option<Contribution> {
    match want {
        Want::Whole => {
            let mut total = 0_i128;
            for row in 0..rows {
                if nulls.is_valid(row) {
                    total += packed.base() + i128::from(packed.code(at(row)));
                }
            }
            Some(Contribution::Whole(total))
        }
        // The scale has to be taken off here the way the flat path takes it off, per row and by
        // the same factor, or a mean over a `DECIMAL(15, 2)` column comes back a hundred times too
        // large. The unscaled integer is what a packed run holds, so the division is the only
        // thing that turns it back into the number the column says it is.
        Want::Real { scale, from } => {
            let factor = pow10(scale) as f64;
            let scaled = scale != 0;
            let mut total = from;
            let mut seen = 0_i64;
            for row in 0..rows {
                if nulls.is_valid(row) {
                    let number = (packed.base() + i128::from(packed.code(at(row)))) as f64;
                    total += if scaled { number / factor } else { number };
                    seen += 1;
                }
            }
            Some(Contribution::Real { total, seen })
        }
        Want::Extreme(least) => {
            let mut found: Option<(usize, u64)> = None;
            for row in 0..rows {
                if !nulls.is_valid(row) {
                    continue;
                }
                let code = packed.code(at(row));
                if found.is_none_or(|(_, held)| if least { code < held } else { code > held }) {
                    found = Some((row, code));
                }
            }
            Some(Contribution::Extreme(found.map(|(row, _)| row)))
        }
    }
}

/// The row that wins an extreme over a dictionary whose values are not a run in memory.
///
/// `MIN` and `MAX` over a text column of a native file used to fall all the way back to the row at
/// a time path, because the dictionary a scan hands over keeps its payload in the file and so has
/// no `Data` to read. That path built one `Value::Varchar` per row, which is an allocation and a
/// copy of the string for every row of the column, and it was about ninety nanoseconds a row.
///
/// Winning is decided on the bytes and nothing else, which [`order`](crate::compare::order) says
/// for both of the types that arrive this way, so this compares the candidate against the bytes of
/// the row that is winning and never builds a value at all. The winner's bytes are kept here rather
/// than read again per row, since reading them can mean going back to the file.
///
/// The check against the code that is already winning is what makes this cheap on real data. A
/// column read out of a file repeats its codes, so most rows never reach the comparison.
///
/// `None` means this declines and the caller falls back, which is what happens for a type that does
/// not order on its bytes and for a code that lands on a dictionary entry that is null.
fn extreme_bytes(
    values: &Vector,
    codes: &[u32],
    nulls: &Validity,
    least: bool,
) -> Option<Option<usize>> {
    if !matches!(values.logical_type(), LogicalType::Varchar | LogicalType::Blob) {
        return None;
    }
    if let Some(ranks) = values.code_ranks() {
        return extreme_ranked(ranks, codes, nulls, least);
    }
    let mut winner: Option<(usize, u32)> = None;
    let mut best: Vec<u8> = Vec::new();
    // row at a time: a dictionary in a file answers one code at a time and there is no run to read.
    for (row, &code) in codes.iter().enumerate() {
        if !nulls.is_valid(row) {
            continue;
        }
        if let Some((_, held)) = winner
            && held == code
        {
            continue;
        }
        let candidate = values.try_bytes_at(code as usize).ok()??;
        let ahead = match winner {
            None => true,
            Some(_) => {
                let ordering = candidate.cmp(best.as_slice());
                if least { ordering.is_lt() } else { ordering.is_gt() }
            }
        };
        if ahead {
            best.clear();
            best.extend_from_slice(candidate);
            winner = Some((row, code));
        }
    }
    Some(winner.map(|(row, _)| row))
}

/// The row that wins an extreme, out of the sorted order the file wrote beside the dictionary.
///
/// This is [`extreme_bytes`] over a dictionary that knows which of any two of its values is smaller,
/// and then no value is read at all. The loop above reads one out of the payload for every row whose
/// code is not the code already winning, which on a column like `Referer` is nearly every row,
/// because eight hundred thousand rows there hold four hundred thousand distinct values and a repeat
/// almost never lands next to the thing it repeats. Here a row costs a load out of a map four bytes
/// wide per distinct value and a comparison of two integers.
///
/// A tie keeps the earlier row, which is what the byte loop does, and two rows tie here exactly when
/// they hold the same value, so the row the caller goes on to read is the same row either way.
///
/// `None` declines, for a code with no rank against it, and the caller falls back.
fn extreme_ranked(
    ranks: &[u32],
    codes: &[u32],
    nulls: &Validity,
    least: bool,
) -> Option<Option<usize>> {
    let mut winner: Option<(usize, u32)> = None;
    // row at a time: a code is a number out of the data, so which value it lands on is not known
    // for any row until that row has been read.
    for (row, &code) in codes.iter().enumerate() {
        if !nulls.is_valid(row) {
            continue;
        }
        let &rank = ranks.get(code as usize)?;
        if winner.is_none_or(|(_, held)| if least { rank < held } else { rank > held }) {
            winner = Some((row, rank));
        }
    }
    Some(winner.map(|(row, _)| row))
}

/// The widest dictionary [`group_tally`] copies, and a power of two.
///
/// The copy is per vector and the gather it speeds up is per row, so a dictionary wide enough that
/// copying it costs more than the fifteen hundred or so rows of a vector is one to leave alone.
/// `UserID` has about half a million distinct values, which is where the idea stops working
/// entirely, and the cut is well below that because the copy also has to stay in the first level
/// cache to be worth anything.
const TALLY_LIMIT: usize = 256;

/// The total of a dictionary encoded column, read through a padded copy of the dictionary.
///
/// The loop this replaces is `values[codes[row]]` for every row, and its cost is not the two loads.
/// It is that a code is data, so nothing knows it is inside the dictionary until it has been read,
/// so there is a bounds check and a branch in the middle of the loop on every row. That is about
/// thirteen instructions a value with no unrolling, and on a ten column integer scan of ClickBench's
/// `hits` it was the largest single thing in the program at twenty three percent.
///
/// So the dictionary is copied into an array whose length is a power of two known at compile time,
/// and the code is masked with that length instead of checked against it. Now the index is inside
/// the array by construction, the compiler can see it, the check and the branch are gone and the
/// loop unrolls. About seven instructions a value, and no branch in it that can be mispredicted.
///
/// The array is sized to the dictionary by a ladder rather than fixed at [`TALLY_LIMIT`], because it
/// has to be zeroed before the copy and a two entry dictionary should not pay for a two hundred and
/// fifty six entry one.
///
/// # Why not count the codes instead
///
/// Because it is slower, which is not what the instruction count says. Counting how often each code
/// appears and then taking one product per dictionary entry moves the multiplications off the per
/// row path entirely and measured six percent fewer instructions than the version here. It also
/// measured nine percent slower on one thread and twenty eight percent slower on thirty two, because
/// the row loop becomes a read modify write against memory at an address that comes out of the data.
/// Four counters per entry were not enough to keep consecutive rows off the same address, and every
/// repeat waits for the previous store to forward. A load has no such problem, and the loop here is
/// loads.
///
/// `None` when the dictionary is too wide to copy, and for a layout with no fixed width. A code past
/// the end of the dictionary reads one of the zeros the copy was padded with rather than raising,
/// and cannot happen: `Vector::dictionary` refuses one on the way in, which is the same invariant
/// the gather this replaces was already relying on to index without a bound of its own.
fn tally(data: &Data, codes: &[u32]) -> Option<i128> {
    // Each rung covers dictionaries up to its own size, and every rung is a power of two so that the
    // mask below is the bounds check.
    match data.len() {
        0..=8 => tally_into::<8>(data, codes),
        9..=32 => tally_into::<32>(data, codes),
        33..=128 => tally_into::<128>(data, codes),
        129..=TALLY_LIMIT => tally_into::<TALLY_LIMIT>(data, codes),
        _ => None,
    }
}

/// [`group_tally`] with the rung it decided on, `SLOTS` entries wide.
fn tally_into<const SLOTS: usize>(data: &Data, codes: &[u32]) -> Option<i128> {
    macro_rules! padded {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => padded!(@run values, $zero),)+
                _ => return None,
            }
        };
        (@run $values:expr, $zero:expr) => {{
            let values = $values.as_slice();
            if values.len() > SLOTS {
                return None;
            }
            let mut table = [$zero; SLOTS];
            table[..values.len()].copy_from_slice(values);
            let mut total: i128 = 0;
            // row at a time: this is the loop the whole function is. Masked rather than bounds
            // checked, which is the point: `SLOTS` is a power of two and is the length of `table`,
            // so the index is inside it by construction and the compiler can see that without any
            // unsafe code.
            for &code in codes {
                total += i128::from(table[code as usize & (SLOTS - 1)]);
            }
            total
        }};
    }
    Some(rudb_vector::for_each_layout!(narrow, padded))
}

/// The first `rows` values as one slice, when the mapping into them is the identity.
///
/// This is the whole of what `DIRECT` buys. A loop written as `values[at(index)]` has to check the
/// index against the length on every row, because nothing in the loop tells the compiler that `at`
/// answers something in range, and on a scan that check was most of what the sum cost. A loop over
/// a slice has the length in hand before it starts, so there is no check, the trip count is known
/// and the addition can go four at a time.
///
/// `None` when the mapping is not the identity, and also when the run is shorter than the rows
/// asked for, which is a caller whose data and length disagree and which goes the careful way
/// rather than panicking.
fn straight<const DIRECT: bool, T>(values: &[T], rows: usize) -> Option<&[T]> {
    if DIRECT { values.get(..rows) } else { None }
}

/// `DIRECT` says `at` is the identity, which is the flat case and is the one worth writing twice.
fn collect<const DIRECT: bool, M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
    want: Want,
) -> Option<Contribution> {
    match want {
        Want::Whole => whole_sum::<DIRECT, M>(data, at, rows, nulls).map(Contribution::Whole),
        Want::Real { scale, from } => real_sum::<DIRECT, M>(data, at, rows, nulls, scale, from),
        Want::Extreme(least) => {
            extreme::<DIRECT, M>(data, at, rows, nulls, least).map(Contribution::Extreme)
        }
    }
}

/// The total of the rows that are not null, as an exact number.
///
/// The accumulator is an `i128` and the widest thing read into it is sixty four bits, so a vector
/// would have to be about 2^63 rows long before its own total could overflow. That is what lets the
/// only overflow check be the one where this total meets the running total, which in turn is what
/// lets the loop vectorize at all.
fn whole_sum<const DIRECT: bool, M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
) -> Option<i128> {
    macro_rules! summed {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                $(Data::$variant(values) => summed!(@run values),)+
                // A total of hugeints can overflow inside one vector, so that one is counted with
                // a check per row just below. An unsigned hugeint has no exact accumulator here at
                // all and goes the row at a time way, which is the way that reports it.
                Data::Int128(values) => wide_sum::<DIRECT, _>(values.as_slice(), &at, rows, nulls)?,
                _ => return None,
            }
        };
        (@run $values:expr) => {{
            let values = $values.as_slice();
            let run = straight::<DIRECT, _>(values, rows);
            let mut total: i128 = 0;
            match nulls {
                Validity::AllValid => match run {
                    Some(run) => {
                        for &value in run {
                            total += i128::from(value);
                        }
                    }
                    None => {
                        for index in 0..rows {
                            total += i128::from(values[at(index)]);
                        }
                    }
                },
                Validity::AllInvalid => {}
                Validity::Mask(mask) => {
                    // A word of the mask at a time, and a conditional move rather than a branch
                    // inside it, because the rows a filter leaves behind are in no pattern a branch
                    // predictor is going to learn.
                    for start in (0..rows).step_by(64) {
                        let word = mask.word(start / 64);
                        for index in start..(start + 64).min(rows) {
                            let number = match run {
                                Some(run) => i128::from(run[index]),
                                None => i128::from(values[at(index)]),
                            };
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

/// The same total for a column of hugeints, which is what a wide decimal is held as.
///
/// A vector of 128 bit values can overflow its own total, so this one checks each addition and
/// hands the vector back when one does. The row at a time path then adds the same rows and raises
/// on the row that overflows, so where the error comes from does not move. The check costs this
/// loop the vectorization the narrow ones get, and it is still one pass over a slice against a
/// `Value` built per row, which is what it replaces. This is q11's ungrouped sum over
/// `DECIMAL(34, 2)`.
fn wide_sum<const DIRECT: bool, M: Fn(usize) -> usize>(
    values: &[i128],
    at: M,
    rows: usize,
    nulls: &Validity,
) -> Option<i128> {
    let run = straight::<DIRECT, _>(values, rows);
    let mut total: i128 = 0;
    for index in 0..rows {
        if !nulls.is_valid(index) {
            continue;
        }
        let number = match run {
            Some(run) => run[index],
            None => values[at(index)],
        };
        total = total.checked_add(number)?;
    }
    Some(total)
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
fn real_sum<const DIRECT: bool, M: Fn(usize) -> usize>(
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
            let values = $values.as_slice();
            let convert = $convert;
            let run = straight::<DIRECT, _>(values, rows);
            let mut total = from;
            let mut seen: i64 = 0;
            // Which rows count is decided once for the vector rather than once per row. A match on
            // the validity enum inside the loop is three quarters of a nanosecond a row on top of
            // an addition that takes one, which is a thing worth finding out by measuring.
            match nulls {
                Validity::AllValid => {
                    match run {
                        Some(run) => {
                            for &value in run {
                                let number = convert(value);
                                total += if scaled { number / factor } else { number };
                            }
                        }
                        None => {
                            for index in 0..rows {
                                let number = convert(values[at(index)]);
                                total += if scaled { number / factor } else { number };
                            }
                        }
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
                            let number = match run {
                                Some(run) => convert(run[index]),
                                None => convert(values[at(index)]),
                            };
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

/// How many rows the value only loop of [`blocked`] covers before it looks for the row.
///
/// Small enough that searching one block for the value it just found is a small part of the pass
/// that found it, and large enough that the outer loop's own arithmetic is noise. A run that arrives
/// in the order the extreme wants is the case that searches every block, and at a thousand that is a
/// sixteenth of a vector searched at a time.
const BLOCK: usize = 1024;

/// Whether a value replaces the best one so far, which is the only place the direction is read.
///
/// `LEAST` is a constant rather than an argument because this sits inside the loop. As an argument
/// it is a test per row that the compiler has to hoist out; as a constant the loop that calls it is
/// a plain minimum or a plain maximum, and the vectorizer knows both of those.
fn beats<const LEAST: bool, T: Ord>(value: T, mark: T) -> bool {
    if LEAST { value < mark } else { value > mark }
}

/// The row holding the smallest or largest value in a run, in two passes over one block.
///
/// A loop that carries the row number of the best value so far cannot go four rows at a time,
/// because that row number is carried from one row to the next and a vector register has no lane to
/// carry it in. A loop that carries only the value can. So this carries only the value across a
/// block of rows and then looks for that value in the one block that produced it, which is one
/// vectorized pass over everything and a second pass over the blocks that improved on what came
/// before. Any row holding the value answers, because two rows with the same number in them give
/// back the same value, and that is what makes the second pass a search rather than a record of
/// where the first pass had got to.
fn blocked<const LEAST: bool, T: Copy + Ord>(run: &[T]) -> Option<usize> {
    let mut mark = *run.first()?;
    let mut held = None;
    for (number, block) in run.chunks(BLOCK).enumerate() {
        let mut best = block[0];
        for &value in &block[1..] {
            best = if beats::<LEAST, T>(value, best) { value } else { best };
        }
        if held.is_none() || beats::<LEAST, T>(best, mark) {
            // The value was read out of this block, so the search finds it and the zero is not
            // reachable. It is a fallback rather than an unwrap because a kernel that panics on a
            // row of data is worse than one that answers with the first row of the block.
            let inside = block.iter().position(|value| *value == best).unwrap_or(0);
            mark = best;
            held = Some(number * BLOCK + inside);
        }
    }
    held
}

/// Which row holds the extreme, with the column's own width and the direction both settled.
///
/// The best so far is held as a `T` rather than widened to an `i128`. One loop for every width was
/// what the widening bought, and it cost a 128 bit comparison per row, which is a pair of
/// instructions the compiler cannot put in a vector register. `T` is the width, so the loop is
/// written once and compiled per width instead.
fn chase<const DIRECT: bool, const LEAST: bool, T: Copy + Ord, M: Fn(usize) -> usize>(
    values: &[T],
    at: &M,
    rows: usize,
    nulls: &Validity,
) -> Option<usize> {
    let run = straight::<DIRECT, _>(values, rows);
    match (nulls, run) {
        (Validity::AllValid, Some(run)) => blocked::<LEAST, T>(run),
        (Validity::AllValid, None) => {
            if rows == 0 {
                return None;
            }
            // The winner is a row number and a number, not an `Option` of a pair. Carrying the
            // option into the loop puts a discriminant test on every row, and the first row is the
            // only row that needs one, so the seed is the first row and the loop starts after it.
            let mut mark = values[at(0)];
            let mut held = 0;
            for index in 1..rows {
                let number = values[at(index)];
                if beats::<LEAST, T>(number, mark) {
                    mark = number;
                    held = index;
                }
            }
            Some(held)
        }
        (Validity::AllInvalid, _) => None,
        (Validity::Mask(mask), run) => {
            // A word of the mask at a time, because the rows a filter leaves behind are in no
            // pattern a branch predictor is going to learn. The seed is an option here, because
            // which row is the first one that is not null is not known before the mask is read.
            let mut best: Option<(usize, T)> = None;
            for start in (0..rows).step_by(64) {
                let word = mask.word(start / 64);
                for index in start..(start + 64).min(rows) {
                    if word >> (index - start) & 1 == 0 {
                        continue;
                    }
                    let number = match run {
                        Some(run) => run[index],
                        None => values[at(index)],
                    };
                    if best.is_none_or(|(_, mark)| beats::<LEAST, T>(number, mark)) {
                        best = Some((index, number));
                    }
                }
            }
            best.map(|(index, _)| index)
        }
    }
}

/// Which row holds the smallest or largest number, or none if every row is null.
fn extreme<const DIRECT: bool, M: Fn(usize) -> usize>(
    data: &Data,
    at: M,
    rows: usize,
    nulls: &Validity,
    least: bool,
) -> Option<Option<usize>> {
    macro_rules! best {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            match data {
                // The direction is settled here, once for the whole vector, so that the loops it
                // reaches have it as a constant. See [`beats`].
                $(Data::$variant(values) => if least {
                    chase::<DIRECT, true, $native, M>(values.as_slice(), &at, rows, nulls)
                } else {
                    chase::<DIRECT, false, $native, M>(values.as_slice(), &at, rows, nulls)
                },)+
                // A float orders NaN the way the comparison kernel says rather than the way the
                // hardware does, and a string extreme is a comparison of bytes rather than of
                // numbers. Both are worth a loop of their own and neither gets a wrong one here.
                // The unsigned hugeint is not in this group, and now that the best so far is held
                // at the column's own width there is nothing in the loop that rules it out, so it
                // waits for a query that asks for one.
                _ => return None,
            }
        };
    }
    Some(rudb_vector::for_each_layout!(exact, best))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grouped_accumulator_does_not_carry_a_full_logical_type() {
        // A LogicalType can own a nested schema. Keeping one in every aggregate state cost more
        // than a hundred MiB on ClickBench q33 before the return was narrowed to Return.
        assert!(size_of::<Accumulator>() <= 32, "{} bytes", size_of::<Accumulator>());
    }

    /// A run's total split into two sums is the total an `i128` gives, at the ends of every width.
    #[test]
    fn a_run_total_is_exact_at_the_ends_of_its_width() {
        fn plain<T: Copy + Into<i128>>(run: &[T]) -> i128 {
            run.iter().map(|&value| value.into()).sum()
        }
        let signed = [i64::MIN, i64::MAX, -1, 0, 1, i64::MIN, i64::MIN, 1 << 32, -(1 << 32) - 7];
        assert_eq!(run_total(&signed), Some(plain(&signed)));
        let unsigned = [u64::MAX, u64::MAX, 0, 1 << 63, u64::from(u32::MAX)];
        assert_eq!(run_total(&unsigned), Some(plain(&unsigned)));
        let narrow = [i32::MIN, i32::MAX, -3, 900];
        assert_eq!(run_total(&narrow), Some(plain(&narrow)));
        let many = vec![i64::MIN; 10_000];
        assert_eq!(run_total(&many), Some(plain(&many)));
        assert_eq!(run_total::<i64>(&[]), Some(0));
        assert_eq!(run_total(&[i128::MAX, 1]), None);
    }

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

    /// A run of more than one block agrees with a row at a time, wherever the winner is in it.
    ///
    /// [`blocked`] reduces a block of rows to a value and then looks for that value in the block it
    /// came from, so the shapes that catch a wrong block number, or a search that looked in the
    /// wrong block, are runs longer than one block with the winner at the front, at the back, and in
    /// the short block past the end of the whole ones. Ascending is the order that improves on every
    /// block and descending the order that improves on none of them after the first.
    #[test]
    fn an_extreme_over_more_than_one_block_finds_the_winner_wherever_it_is() {
        for order in ["ascending", "descending", "scattered", "all one value"] {
            let number = |row: usize, rows: usize| match order {
                "ascending" => row as i64,
                "descending" => (rows - row) as i64,
                "scattered" => ((row * 7919) % rows) as i64,
                _ => 42,
            };
            for nulls in [0_usize, 5, 1] {
                // Two batches, the first a block and a bit and the second most of a block, so that
                // a winner carried from one vector to the next is read as well as one inside a
                // vector.
                let batches: Vec<Vector> = [BLOCK + 7, BLOCK - 3]
                    .iter()
                    .map(|&rows| {
                        let values: Vec<Value> = (0..rows)
                            .map(|row| {
                                if nulls > 0 && row % nulls == 0 {
                                    Value::Null
                                } else {
                                    Value::BigInt(number(row, rows))
                                }
                            })
                            .collect();
                        Vector::from_values(LogicalType::BigInt, &values).expect("bigints")
                    })
                    .collect();
                for name in ["min", "max"] {
                    let note = format!("{name} over a run {order}, one null in {nulls}");
                    agrees(name, &LogicalType::BigInt, &batches, &note);
                }
            }
        }
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
        let error = Accumulator::new("bitstring_agg", &LogicalType::Double)
            .expect_err("bitstring_agg is not written yet");
        assert!(error.message().contains("the bitstring_agg aggregate"), "{error}");
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
            LogicalType::Date => Value::Date(number as i32),
            // A time of day is not negative, so this one takes the magnitude rather than the
            // number, which is the same thing the unsigned types above do.
            LogicalType::Time => Value::Time(positive as i64),
            LogicalType::Timestamp => Value::Timestamp(number),
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
            // The three types a `min` and a `max` read as the integer they are stored as. They
            // were left out when this test was written, and what the gap cost is in [`feed_of`].
            LogicalType::Date,
            LogicalType::Time,
            LogicalType::Timestamp,
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

    /// The states the two tests above build, handed back rather than finished.
    fn scattered_states(
        name: &str,
        returns: &LogicalType,
        batches: &[(Vector, Vec<usize>)],
        groups: usize,
        reads: bool,
    ) -> Result<Vec<Accumulator>> {
        let mut states = Vec::new();
        for _ in 0..groups * STRIDE {
            states.push(Accumulator::new(name, returns)?);
        }
        for (batch, slots) in batches {
            let input = reads.then_some(batch);
            update_scattered(&mut states, slots, STRIDE, OFFSET, input, slots.len())?;
        }
        Ok(states)
    }

    /// The run at a time finish against the `Value` at a time one, over every aggregate and type.
    ///
    /// [`finish_run`] is allowed to decline a shape. It is not allowed to answer differently from
    /// the loop it replaces on a shape it takes, and that is the whole of what this checks: the same
    /// states finished both ways, compared row by row with the nulls, at three null densities so
    /// that a group which saw nothing and a group which saw only nulls are both in there.
    ///
    /// The second half of the assertion matters as much as the first. `min` and `max` are expected
    /// to decline, every other aggregate over every numeric type is expected to be taken, and a
    /// change that quietly stops covering `sum` would otherwise pass this test by falling back.
    #[test]
    fn the_run_at_a_time_finish_answers_what_the_value_at_a_time_finish_answers() {
        let mut rng = Rng(0x5eed_ca11_ab1e_00f1);
        let groups = 5;
        let types = [
            LogicalType::TinyInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UBigInt,
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::decimal(9, 2).expect("a legal decimal"),
            LogicalType::decimal(30, 6).expect("a legal decimal"),
            LogicalType::Varchar,
        ];
        for ty in &types {
            for name in ["count_star", "count", "sum", "avg", "min", "max"] {
                let returns = returns_of(name, ty);
                let reads = name != "count_star";
                for nulls in [0_usize, 4, 1] {
                    let batch = flat(ty, 97, nulls, &mut rng);
                    let slots = deal(batch.len(), groups);
                    let dealt = vec![(batch, slots)];
                    let note = format!("{name} over {ty}, one null in {nulls}");
                    let Ok(states) = scattered_states(name, &returns, &dealt, groups, reads) else {
                        continue;
                    };
                    let at: Vec<usize> = (0..groups).collect();
                    let slow: Vec<Value> = at
                        .iter()
                        .map(|&group| states[group * STRIDE + OFFSET].finish())
                        .collect::<Result<_>>()
                        .expect("the value at a time finish answers for every shape here");
                    let fast = finish_run(&states, &at, STRIDE, OFFSET, &returns)
                        .expect("no total here is out of range");
                    let owns = matches!(name, "min" | "max");
                    assert_eq!(fast.is_none(), owns, "{note}: taken when it should not be");
                    let Some(fast) = fast else { continue };
                    assert_eq!(fast.len(), groups, "{note}");
                    assert_eq!(fast.logical_type(), &returns, "{note}");
                    for (group, slow) in slow.iter().enumerate() {
                        assert_eq!(&fast.value_at(group), slow, "{note}, group {group}");
                    }
                }
            }
        }
    }

    /// The groups the caller picked, in the order it picked them, which is what a `HAVING` or a
    /// `LIMIT` over a partition hands down. The run at a time finish reads the slots rather than a
    /// range, so an order that is not slot order has to come out in the order it was asked for.
    #[test]
    fn the_run_at_a_time_finish_follows_the_slots_it_is_given() {
        let mut rng = Rng(0x5eed_ca11_ab1e_00f2);
        let groups = 5;
        let ty = LogicalType::BigInt;
        let returns = returns_of("sum", &ty);
        let batch = flat(&ty, 97, 4, &mut rng);
        let slots = deal(batch.len(), groups);
        let states = scattered_states("sum", &returns, &[(batch, slots)], groups, true)
            .expect("a sum over BIGINT builds");
        let at = [3_usize, 0, 4];
        let fast = finish_run(&states, &at, STRIDE, OFFSET, &returns)
            .expect("no total here is out of range")
            .expect("a sum over BIGINT is a shape the run at a time finish takes");
        for (row, &group) in at.iter().enumerate() {
            let slow = states[group * STRIDE + OFFSET].finish().expect("the sum finishes");
            assert_eq!(fast.value_at(row), slow, "row {row} is group {group}");
        }
    }

    /// Every aggregate over every type, dealt out into five groups, against one accumulator each.
    ///
    /// This is the invariant the whole scattered path rests on and it is the same invariant the
    /// vector at a time test above asserts: a faster loop that reaches a different answer is not an
    /// answer. Two batches rather than one, because a state that is restarted at every vector is
    /// right on one vector and wrong on the query, and the slots change between them so no group
    /// sees the same rows twice.
    ///
    /// It runs twice, at five groups and at more than [`FEW`], because those are two different
    /// loops: the first adds a chunk up per group before it touches a state, and the second reaches
    /// the state of every row.
    #[test]
    fn every_aggregate_scattered_into_groups_agrees_with_one_accumulator_per_group() {
        scattered_into(5, 0x5eed_ca11_ab1e_0061);
        scattered_into(FEW + 3, 0x5eed_ca11_ab1e_0062);
    }

    fn scattered_into(groups: usize, seed: u64) {
        let mut rng = Rng(seed);
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
            // The same three as above, and this is the test that covers the loop they now take,
            // since the widening in [`feed_of`] is about the scattered path.
            LogicalType::Date,
            LogicalType::Time,
            LogicalType::Timestamp,
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
                    let coded =
                        Vector::dictionary(codes.clone(), first.clone()).expect("codes in range");
                    // The two forms our own file hands a column of numbers out in. A float, a
                    // string and a column whose range is too wide to pay for a packing all hand
                    // the vector back as it was, which makes those the flat case a second time.
                    let packed = first.bit_packed().expect("packs or hands the vector back");
                    let over_packed =
                        Vector::dictionary(codes, packed.clone()).expect("codes in range");
                    for (shape, batches) in [
                        ("flat", vec![first.clone(), second.clone()]),
                        ("dictionary", vec![coded, second.clone()]),
                        ("packed", vec![packed, second.clone()]),
                        ("dictionary over packed", vec![over_packed, second.clone()]),
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

    /// A run at a time answers what a row at a time answers, over every aggregate and type, and
    /// takes every count and every exact total and mean over a column with no nulls in it.
    #[test]
    fn a_run_at_a_time_agrees_with_a_row_at_a_time() {
        let mut rng = Rng(0x5eed_0f00_2c0d_0028);
        let groups = 4;
        let types = [
            LogicalType::TinyInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UBigInt,
            LogicalType::Double,
            LogicalType::decimal(18, 4).expect("a legal decimal"),
            LogicalType::decimal(30, 6).expect("a legal decimal"),
            LogicalType::Date,
            LogicalType::Varchar,
        ];
        // Runs of one slot in no order, with a run that belongs to nothing among them.
        let slots: Vec<usize> = [(2, 30), (0, 11), (NOWHERE, 9), (3, 1), (0, 20), (1, 26)]
            .iter()
            .flat_map(|&(slot, length)| std::iter::repeat_n(slot, length))
            .collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (row, &slot) in slots.iter().enumerate() {
            match runs.last_mut() {
                Some((last, end)) if *last == slot => *end = row + 1,
                _ => runs.push((slot, row + 1)),
            }
        }
        for ty in &types {
            for name in ["count_star", "count", "sum", "avg", "min", "max"] {
                let returns = returns_of(name, ty);
                let reads = name != "count_star";
                for nulls in [0_usize, 4] {
                    let batch = flat(ty, slots.len(), nulls, &mut rng);
                    let note = format!("{name} over {ty}, one null in {nulls}");
                    let dealt = vec![(batch.clone(), slots.clone())];
                    let slow = group_at_a_time(name, &returns, &dealt, groups, reads);
                    let mut states = Vec::new();
                    for _ in 0..groups * STRIDE {
                        states.push(Accumulator::new(name, &returns).expect("known"));
                    }
                    let input = reads.then_some(&batch);
                    let took = update_runs(&mut states, &runs, STRIDE, OFFSET, input, slots.len());
                    let taken = took.as_ref().ok().copied();
                    let fast = took.and_then(|took| {
                        if !took {
                            update_scattered(
                                &mut states,
                                &slots,
                                STRIDE,
                                OFFSET,
                                input,
                                slots.len(),
                            )?;
                        }
                        (0..groups)
                            .map(|group| states[group * STRIDE + OFFSET].finish())
                            .collect::<Result<Vec<_>>>()
                    });
                    match (slow, fast) {
                        (Ok(slow), Ok(fast)) => assert_eq!(slow, fast, "{note}"),
                        (Err(slow), Err(fast)) => {
                            assert_eq!(slow.message(), fast.message(), "{note}");
                        }
                        (slow, fast) => panic!("{note}: {slow:?} against {fast:?}"),
                    }
                    let exact = ty.is_integer() || matches!(ty, LogicalType::Decimal { .. });
                    let covered = name == "count_star"
                        || (nulls == 0
                            && (name == "count" || (exact && matches!(name, "sum" | "avg"))));
                    if covered {
                        assert_eq!(taken, Some(true), "{note} is taken a run at a time");
                    }
                }
            }
        }
    }

    /// [`few_runs`] keeps its per group total in an `i64`, and a column of 128 bit values is where
    /// that runs out. Both ways it can run out are here: a single value too large to become an `i64`
    /// at all, and a pile of values that each fit and whose total does not. Either hands the call
    /// back to the run loop, and the answer has to be the same one a row at a time reaches, which is
    /// what the handback being free of consequence means.
    #[test]
    fn a_total_too_large_for_an_i64_local_still_answers_what_a_row_at_a_time_answers() {
        let rows = 40;
        let slots: Vec<usize> = (0..rows).map(|row| row / 10).collect();
        let runs: Vec<(usize, usize)> = (0..4).map(|group| (group, (group + 1) * 10)).collect();
        let huge = i128::from(i64::MAX);
        let cases = [
            ("one value past an i64", vec![Value::HugeInt(huge + 1); rows]),
            ("a total past an i64", vec![Value::HugeInt(huge / 4); rows]),
        ];
        for (note, values) in &cases {
            let column = Vector::from_values(LogicalType::HugeInt, values).expect("huge integers");
            for name in ["sum", "avg"] {
                let returns = returns_of(name, &LogicalType::HugeInt);
                let fresh = || {
                    let mut states = Vec::new();
                    for _ in 0..4 * STRIDE {
                        states.push(Accumulator::new(name, &returns).expect("known"));
                    }
                    states
                };
                let (mut by_run, mut by_row) = (fresh(), fresh());
                let took = update_runs(&mut by_run, &runs, STRIDE, OFFSET, Some(&column), rows)
                    .expect("folds them in");
                assert!(took, "{name} over {note} is still taken a run at a time");
                update_scattered(&mut by_row, &slots, STRIDE, OFFSET, Some(&column), rows)
                    .expect("folds them in");
                for group in 0..4 {
                    let at = group * STRIDE + OFFSET;
                    assert_eq!(
                        by_run[at].finish().expect("finishes"),
                        by_row[at].finish().expect("finishes"),
                        "{name} over {note}, group {group}"
                    );
                }
            }
        }
    }

    /// A chunk shaped like q01's: a sum and a mean of a column held in an `i64` and a sum and a mean
    /// of one held in an `i128`, with a min and a `COUNT(*)` beside them that cannot share a walk.
    ///
    /// Two layouts is the case worth building, because a pass covers one and the caller is meant to
    /// ask again for what is left. So this asserts which calls the sharing took as well as what they
    /// answered: a pass that quietly took one call, or none, would answer exactly the same and would
    /// be the whole of the change doing nothing.
    #[test]
    fn many_calls_over_one_walk_of_the_runs_answer_what_a_call_at_a_time_answers() {
        let mut rng = Rng(0x5eed_c0de_5add_0050);
        let rows = 97;
        let groups = 4;
        let money = LogicalType::decimal(15, 2).expect("a legal decimal");
        let wide = LogicalType::decimal(30, 4).expect("a legal decimal");
        let calls = [
            ("sum", &money),
            ("avg", &money),
            ("sum", &wide),
            ("avg", &wide),
            ("min", &money),
            ("count_star", &LogicalType::BigInt),
        ];
        let stride = calls.len();
        // A row in no group and a run of one row are both in here, and the runs are cut off the slots
        // rather than written out, so the two paths are handed the same chunk however it comes out.
        let slots: Vec<usize> =
            (0..rows).map(|row| if row % 23 == 7 { NOWHERE } else { row / 7 % groups }).collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (row, &slot) in slots.iter().enumerate() {
            match runs.last_mut() {
                Some((held, end)) if *held == slot => *end = row + 1,
                _ => runs.push((slot, row + 1)),
            }
        }
        let columns: Vec<Option<Vector>> = calls
            .iter()
            .map(|&(name, ty)| (name != "count_star").then(|| flat(ty, rows, 0, &mut rng)))
            .collect();
        let inputs: Vec<Option<&Vector>> = columns.iter().map(Option::as_ref).collect();
        let fresh = || {
            let mut states = Vec::new();
            for _ in 0..groups {
                for &(name, ty) in &calls {
                    states.push(Accumulator::new(name, &returns_of(name, ty)).expect("known"));
                }
            }
            states
        };
        // A call at a time, which is what the sharing has to agree with. A min goes by neither run
        // path, so it takes the row at a time route here as it does in the operator.
        let mut alone = fresh();
        for (at, &input) in inputs.iter().enumerate() {
            if !update_runs(&mut alone, &runs, stride, at, input, rows).expect("folds them in") {
                update_scattered(&mut alone, &slots, stride, at, input, rows)
                    .expect("folds them in");
            }
        }
        let mut together = fresh();
        let offered = (1_u64 << stride) - 1;
        let mut shared = 0;
        loop {
            let took =
                update_shared_runs(&mut together, &runs, stride, &inputs, offered & !shared, rows)
                    .expect("folds them in");
            if took == 0 {
                break;
            }
            assert_eq!(took & shared, 0, "a pass took a call another pass had already taken");
            shared |= took;
        }
        // The two layouts take a pass each, the count rides on the first of them, and the min is left
        // where it was because a run of values it has to compare is not a run it can add up.
        assert_eq!(shared, 0b10_1111, "the wrong calls shared a walk");
        for (at, &input) in inputs.iter().enumerate() {
            if shared >> at & 1 == 1 {
                continue;
            }
            if !update_runs(&mut together, &runs, stride, at, input, rows).expect("folds them in") {
                update_scattered(&mut together, &slots, stride, at, input, rows)
                    .expect("folds them in");
            }
        }
        for group in 0..groups {
            for (at, &(name, _)) in calls.iter().enumerate() {
                let index = group * stride + at;
                assert_eq!(
                    together[index].finish().expect("finishes"),
                    alone[index].finish().expect("finishes"),
                    "{name} at {at} of group {group}"
                );
            }
        }
    }

    /// A chunk shaped like q01's really is: calls over stored decimals, which arrive as a packed run or
    /// as a dictionary over one, and calls over decimals the query's own arithmetic computed, which are
    /// flat because nothing stored them.
    ///
    /// Every answer here is checked against the scatter rather than against the other run path, since
    /// the point is that a column pointing somewhere else now reaches a run path at all. It asserts the
    /// mask too: the coded calls outnumber the flat ones, so they are the first pass and the counts ride
    /// with them, and the flat pair is the second.
    #[test]
    fn a_dictionary_over_a_packed_run_folds_over_the_runs_as_a_flat_column_does() {
        let mut rng = Rng(0x5eed_c0de_dbca_0052);
        let rows = 97;
        let groups = 4;
        let money = LogicalType::decimal(15, 2).expect("a legal decimal");
        let wide = LogicalType::decimal(30, 4).expect("a legal decimal");
        // A count of a column is here as well as a `COUNT(*)`, because a count over a column with no
        // nulls in it is the length of each run whatever form it is held in, and that was refused
        // along with the totals. A packed run is here beside the dictionaries over one, since a stored
        // column arrives as either.
        let calls = [
            ("sum", &money, "coded"),
            ("avg", &money, "coded"),
            ("sum", &money, "packed"),
            ("count", &money, "packed"),
            ("sum", &wide, "flat"),
            ("avg", &wide, "flat"),
            ("count_star", &LogicalType::BigInt, "none"),
        ];
        let stride = calls.len();
        let slots: Vec<usize> =
            (0..rows).map(|row| if row % 23 == 7 { NOWHERE } else { row / 5 % groups }).collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (row, &slot) in slots.iter().enumerate() {
            match runs.last_mut() {
                Some((held, end)) if *held == slot => *end = row + 1,
                _ => runs.push((slot, row + 1)),
            }
        }
        // Thirteen distinct values with the codes walking over them, which is what a stored column of
        // few distinct values is written as and is not the order the rows are in.
        let distinct = 13;
        let packed = |ty: &LogicalType, held: usize, rng: &mut Rng| {
            let values = flat(ty, held, 0, rng).bit_packed().expect("packs or hands it back");
            assert_eq!(values.form(), Form::BitPacked, "the values of {ty} did not pack");
            values
        };
        let coded = |ty: &LogicalType, rng: &mut Rng| {
            let codes: Vec<u32> = (0..rows).map(|row| (row * 7 % distinct) as u32).collect();
            let over =
                Vector::dictionary(codes, packed(ty, distinct, rng)).expect("codes are in range");
            assert_eq!(over.form(), Form::Dictionary);
            over
        };
        let columns: Vec<Option<Vector>> = calls
            .iter()
            .map(|&(_, ty, held)| match held {
                "coded" => Some(coded(ty, &mut rng)),
                "packed" => Some(packed(ty, rows, &mut rng)),
                "flat" => Some(flat(ty, rows, 0, &mut rng)),
                _ => None,
            })
            .collect();
        let inputs: Vec<Option<&Vector>> = columns.iter().map(Option::as_ref).collect();
        let fresh = || {
            let mut states = Vec::new();
            for _ in 0..groups {
                for &(name, ty, _) in &calls {
                    states.push(Accumulator::new(name, &returns_of(name, ty)).expect("known"));
                }
            }
            states
        };
        let mut alone = fresh();
        for (at, &input) in inputs.iter().enumerate() {
            update_scattered(&mut alone, &slots, stride, at, input, rows).expect("folds them in");
        }
        // One call at a time down the run path, which for the three dictionaries is the read and the
        // fold that used to be refused.
        let mut apiece = fresh();
        for (at, &input) in inputs.iter().enumerate() {
            assert!(
                update_runs(&mut apiece, &runs, stride, at, input, rows).expect("folds them in"),
                "the call at {at} was refused by the run path"
            );
        }
        let mut together = fresh();
        let offered = (1_u64 << stride) - 1;
        let mut shared = 0;
        let mut passes = 0;
        loop {
            let took =
                update_shared_runs(&mut together, &runs, stride, &inputs, offered & !shared, rows)
                    .expect("folds them in");
            if took == 0 {
                break;
            }
            assert_eq!(took & shared, 0, "a pass took a call another pass had already taken");
            shared |= took;
            passes += 1;
        }
        assert_eq!(shared, 0b111_1111, "the wrong calls shared a walk");
        assert_eq!(passes, 2, "the coded calls and the flat ones did not take a pass each");
        for group in 0..groups {
            for (at, &(name, _, _)) in calls.iter().enumerate() {
                let index = group * stride + at;
                let note = format!("{name} at {at} of group {group}");
                let answer = alone[index].finish().expect("finishes");
                assert_eq!(apiece[index].finish().expect("finishes"), answer, "{note}, apiece");
                assert_eq!(together[index].finish().expect("finishes"), answer, "{note}, together");
            }
        }
    }

    /// q01's real shape walks the runs once. Its stored decimals are packed and its computed ones are
    /// flat, and both are held in an `i64`, so the read out columns and the flat ones are one pass.
    ///
    /// The walk itself is most of what a pass costs, so what this asserts is the pass count. Two passes
    /// over three columns and two would read the same values and walk the same runs twice.
    #[test]
    fn a_flat_column_of_i64_walks_the_runs_with_the_columns_read_out_into_one() {
        let mut rng = Rng(0x5eed_c0de_dbca_0071);
        let rows = 89;
        let groups = 4;
        let money = LogicalType::decimal(15, 2).expect("a legal decimal");
        let computed = LogicalType::decimal(18, 4).expect("a legal decimal");
        let calls = [
            ("sum", &money, "packed"),
            ("avg", &money, "packed"),
            ("sum", &money, "coded"),
            ("sum", &computed, "flat"),
            ("sum", &computed, "flat"),
            ("count", &money, "packed"),
            ("count_star", &LogicalType::BigInt, "none"),
        ];
        let stride = calls.len();
        let slots: Vec<usize> =
            (0..rows).map(|row| if row % 19 == 5 { NOWHERE } else { row / 3 % groups }).collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (row, &slot) in slots.iter().enumerate() {
            match runs.last_mut() {
                Some((held, end)) if *held == slot => *end = row + 1,
                _ => runs.push((slot, row + 1)),
            }
        }
        let distinct = 11;
        let columns: Vec<Option<Vector>> = calls
            .iter()
            .map(|&(_, ty, held)| match held {
                "packed" => {
                    let values =
                        flat(ty, rows, 0, &mut rng).bit_packed().expect("packs or hands it back");
                    assert_eq!(values.form(), Form::BitPacked, "the values of {ty} did not pack");
                    Some(values)
                }
                "coded" => {
                    let values = flat(ty, distinct, 0, &mut rng)
                        .bit_packed()
                        .expect("packs or hands it back");
                    let codes: Vec<u32> =
                        (0..rows).map(|row| (row * 5 % distinct) as u32).collect();
                    Some(Vector::dictionary(codes, values).expect("codes are in range"))
                }
                "flat" => {
                    let values = flat(ty, rows, 0, &mut rng);
                    assert_eq!(values.form(), Form::Flat);
                    Some(values)
                }
                _ => None,
            })
            .collect();
        let inputs: Vec<Option<&Vector>> = columns.iter().map(Option::as_ref).collect();
        let fresh = || {
            let mut states = Vec::new();
            for _ in 0..groups {
                for &(name, ty, _) in &calls {
                    states.push(Accumulator::new(name, &returns_of(name, ty)).expect("known"));
                }
            }
            states
        };
        let mut alone = fresh();
        for (at, &input) in inputs.iter().enumerate() {
            update_scattered(&mut alone, &slots, stride, at, input, rows).expect("folds them in");
        }
        let mut together = fresh();
        let offered = (1_u64 << stride) - 1;
        let mut shared = 0;
        let mut passes = 0;
        loop {
            let took =
                update_shared_runs(&mut together, &runs, stride, &inputs, offered & !shared, rows)
                    .expect("folds them in");
            if took == 0 {
                break;
            }
            shared |= took;
            passes += 1;
        }
        assert_eq!(shared, 0b111_1111, "the wrong calls shared a walk");
        assert_eq!(passes, 1, "the flat i64 calls did not walk the runs with the read out ones");
        for group in 0..groups {
            for (at, &(name, _, _)) in calls.iter().enumerate() {
                let index = group * stride + at;
                let answer = alone[index].finish().expect("finishes");
                let note = format!("{name} at {at} of group {group}");
                assert_eq!(together[index].finish().expect("finishes"), answer, "{note}");
            }
        }
    }

    /// A shared pass that runs out of room hands every call of it back untouched.
    ///
    /// The locals are `i64` and the column here is 128 bit values too large to become one, which is
    /// the miss [`few_runs`] documents. It matters more here than there because the pass is holding
    /// several calls: one column it cannot read has to leave the other calls of that pass exactly as
    /// they were, or the caller folding them again would count their rows twice.
    #[test]
    fn a_shared_pass_that_runs_out_of_room_leaves_every_call_of_it_alone() {
        let rows = 40;
        let groups = 4;
        let runs: Vec<(usize, usize)> =
            (0..groups).map(|group| (group, (group + 1) * 10)).collect();
        let huge = i128::from(i64::MAX) + 1;
        let stride = 2;
        let small = Vector::from_values(LogicalType::HugeInt, &vec![Value::HugeInt(7); rows])
            .expect("huge integers");
        let past = Vector::from_values(LogicalType::HugeInt, &vec![Value::HugeInt(huge); rows])
            .expect("huge integers");
        let returns = returns_of("sum", &LogicalType::HugeInt);
        let mut states = Vec::new();
        for _ in 0..groups * stride {
            states.push(Accumulator::new("sum", &returns).expect("known"));
        }
        let inputs = [Some(&small), Some(&past)];
        let took = update_shared_runs(&mut states, &runs, stride, &inputs, 0b11, rows)
            .expect("gives up rather than failing");
        assert_eq!(took, 0, "a pass that cannot read one of its columns took the other one anyway");
        for (at, state) in states.iter().enumerate() {
            assert_eq!(state.finish().expect("finishes"), Value::Null, "the state at {at} was fed");
        }
        // The column that fits shares a walk on its own, so what the case above proves is the
        // handback and not that these two could never have shared one.
        let fits = [Some(&small), Some(&small)];
        let took = update_shared_runs(&mut states, &runs, stride, &fits, 0b11, rows)
            .expect("folds them in");
        assert_eq!(took, 0b11, "two columns that fit did not share a walk");
    }

    /// A column at the widest its type holds totals through the shared pass the way the scatter does.
    ///
    /// The walk keeps its totals in an `i64` per call per group, so the row counts and the widths that
    /// interest it are the ones near where that stops being enough room. Every row of the chunk the
    /// largest value its type has and every row in the one group is the most such a chunk can come to,
    /// and the four here are the types whose widest value a few hundred rows of leaves room for.
    ///
    /// A `BIGINT` of `i64::MAX` is the other side of the same line and is why the second half is here.
    /// The case above it reaches the handback with a value too large to become an `i64` at all, which is
    /// one of the two misses [`many_runs`] documents. This one reaches it with values that each fit and
    /// a total that does not, which is the other, and nothing else covered it.
    #[test]
    fn a_column_at_the_widest_its_type_holds_totals_the_way_the_scatter_does() {
        let rows = 300;
        let stride = 2;
        let money = LogicalType::decimal(15, 2).expect("a legal decimal");
        let brim = Value::Decimal { unscaled: -999_999_999_999_999, width: 15, scale: 2 };
        let widest = [
            (&LogicalType::TinyInt, Value::TinyInt(i8::MIN)),
            (&LogicalType::Integer, Value::Integer(i32::MIN)),
            (&LogicalType::UInteger, Value::UInteger(u32::MAX)),
            (&money, brim),
        ];
        let slots = vec![0_usize; rows];
        let runs = [(0_usize, rows)];
        for (ty, value) in widest {
            let column = Vector::from_values(ty.clone(), &vec![value; rows])
                .expect("one value over and over");
            let inputs = [Some(&column), Some(&column)];
            let fresh = || {
                let returns = returns_of("sum", ty);
                (0..stride)
                    .map(|_| Accumulator::new("sum", &returns).expect("known"))
                    .collect::<Vec<_>>()
            };
            let mut alone = fresh();
            for at in 0..stride {
                update_scattered(&mut alone, &slots, stride, at, Some(&column), rows)
                    .expect("folds them in");
            }
            let mut together = fresh();
            let took = update_shared_runs(&mut together, &runs, stride, &inputs, 0b11, rows)
                .expect("folds them in");
            assert_eq!(took, 0b11, "two columns of {ty} did not share a walk");
            for at in 0..stride {
                assert_eq!(
                    together[at].finish().expect("finishes"),
                    alone[at].finish().expect("finishes"),
                    "the call at {at} over {ty}"
                );
            }
        }
        let past = Vector::from_values(LogicalType::BigInt, &vec![Value::BigInt(i64::MAX); rows])
            .expect("big integers");
        let inputs = [Some(&past), Some(&past)];
        let returns = returns_of("sum", &LogicalType::BigInt);
        let mut states: Vec<Accumulator> =
            (0..stride).map(|_| Accumulator::new("sum", &returns).expect("known")).collect();
        let took = update_shared_runs(&mut states, &runs, stride, &inputs, 0b11, rows)
            .expect("gives up rather than failing");
        assert_eq!(took, 0, "a total that left an i64 was folded in anyway");
        for (at, state) in states.iter().enumerate() {
            assert_eq!(state.finish().expect("finishes"), Value::Null, "the state at {at} was fed");
        }
    }

    /// A sum read out of a mean's state over the same column is the sum a call of its own reaches,
    /// over every type a mean keeps an exact total for, with nulls among the rows and without, and
    /// with rows in no group. Where the mean has no exact total to give, the sum of its own had
    /// nowhere to put one either and says so.
    #[test]
    fn a_sum_read_out_of_a_mean_is_the_sum_a_call_of_its_own_reaches() {
        let mut rng = Rng(0x5eed_0f00_2c0d_0031);
        let types = [
            LogicalType::TinyInt,
            LogicalType::Integer,
            LogicalType::BigInt,
            LogicalType::HugeInt,
            LogicalType::UBigInt,
            LogicalType::decimal(18, 4).expect("a legal decimal"),
            LogicalType::decimal(30, 6).expect("a legal decimal"),
        ];
        let rows = 97;
        let slots: Vec<usize> =
            (0..rows).map(|row| if row % 11 == 4 { NOWHERE } else { row % 3 }).collect();
        for ty in &types {
            for nulls in [0_usize, 5] {
                let column = flat(ty, rows, nulls, &mut rng);
                let sums = returns_of("sum", ty);
                let means = returns_of("avg", ty);
                let mut summed = vec![Accumulator::new("sum", &sums).expect("known"); 3];
                let mut meant = vec![Accumulator::new("avg", &means).expect("known"); 3];
                let folded = update_scattered(&mut summed, &slots, 1, 0, Some(&column), rows);
                update_scattered(&mut meant, &slots, 1, 0, Some(&column), rows).expect("folds");
                for group in 0..3 {
                    let note = format!("sum over {ty}, one null in {nulls}, group {group}");
                    let Some((total, seen)) = meant[group].exact_total() else {
                        assert!(folded.is_err(), "{note}: the mean is inexact and the sum is not");
                        continue;
                    };
                    let read = Accumulator::sum_of(total, seen, &sums).expect("a whole total");
                    let said =
                        |answer: Result<Value>| answer.map_err(|at| at.message().to_string());
                    let of_its_own = match &folded {
                        Err(error) => Err(error.message().to_string()),
                        Ok(()) => said(summed[group].finish()),
                    };
                    assert_eq!(said(read.finish()), of_its_own, "{note}");
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

    /// A chunk's tally stands in for the counts a sum, a mean and a count take as they go, and
    /// gives the same answers, with nulls in the argument or not and with rows in no group.
    #[test]
    fn a_tally_taken_once_answers_what_counting_every_call_answers() {
        let rows = 64;
        let slots: Vec<usize> =
            (0..rows).map(|row| if row % 7 == 3 { NOWHERE } else { row * 5 % 3 }).collect();
        let full: Vec<Value> = (0..rows).map(|row| Value::Integer(row as i32 - 20)).collect();
        let holed: Vec<Value> = full
            .iter()
            .enumerate()
            .map(|(row, value)| if row % 5 == 0 { Value::Null } else { value.clone() })
            .collect();
        let tally = group_tally(&slots, 3).expect("three groups is few");
        assert_eq!(
            tally.iter().sum::<i64>(),
            slots.iter().filter(|&&s| s != NOWHERE).count() as i64
        );
        for values in [&full, &holed] {
            let column = Vector::from_values(LogicalType::Integer, values).expect("integers");
            for name in ["sum", "avg", "count", "count_star"] {
                let fresh =
                    || vec![Accumulator::new(name, &LogicalType::Integer).expect("known"); 3];
                let (mut counting, mut tallied) = (fresh(), fresh());
                let input = (name != "count_star").then_some(&column);
                update_scattered(&mut counting, &slots, 1, 0, input, rows).expect("folds");
                update_tallied(&mut tallied, &slots, Some(&tally), 1, 0, input, rows)
                    .expect("folds");
                for (one, other) in counting.iter().zip(&tallied) {
                    assert_eq!(one.finish().expect("finishes"), other.finish().expect("finishes"));
                }
            }
        }
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
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 0);
        // A date, a time and a timestamp are each one signed integer of one unit, so the loop that
        // compares a run of them answers a `min` the same way it answers a `min` over an
        // `INTEGER`. They were not on this list once and a grouped `min` over `l_shipdate` cost
        // 776 instructions a row rather than 90 because of it.
        for (ty, values, least) in [
            (
                LogicalType::Date,
                vec![Value::Date(3), Value::Date(1), Value::Date(2)],
                Value::Date(2),
            ),
            (
                LogicalType::Time,
                vec![Value::Time(30), Value::Time(10), Value::Time(20)],
                Value::Time(20),
            ),
            (
                LogicalType::Timestamp,
                vec![Value::Timestamp(30), Value::Timestamp(10), Value::Timestamp(20)],
                Value::Timestamp(20),
            ),
        ] {
            let column = Vector::from_values(ty.clone(), &values).expect("a vector of this type");
            for name in ["min", "max"] {
                fallback::reset();
                let mut states = vec![Accumulator::new(name, &ty).expect("known"); 2];
                update_scattered(&mut states, &slots, 1, 0, Some(&column), 3)
                    .expect("folds them in");
                assert_eq!(
                    fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat),
                    0,
                    "{name} over a {ty} took the row at a time path"
                );
            }
            // Rows nought and two are the group, and the smaller of the two is the third value.
            let mut states = vec![Accumulator::new("min", &ty).expect("known"); 2];
            update_scattered(&mut states, &slots, 1, 0, Some(&column), 3).expect("folds them in");
            assert_eq!(states[0].finish().expect("finishes"), least);
        }
    }

    /// The two forms our own storage hands out for a column of numbers, a packed run and a
    /// dictionary over one. Every aggregate over both against the row at a time path, and the
    /// counter to say that neither of them reached it.
    #[test]
    fn a_packed_run_and_a_dictionary_over_one_do_not_reach_the_row_at_a_time_path() {
        fallback::reset();
        let ty = LogicalType::decimal(15, 2).expect("a legal decimal");
        let values: Vec<Value> = (0..24)
            .map(|row| {
                if row % 7 == 0 {
                    Value::Null
                } else {
                    Value::Decimal { unscaled: 900 + row * 13, width: 15, scale: 2 }
                }
            })
            .collect();
        let packed = Vector::from_values(ty.clone(), &values)
            .expect("a flat decimal")
            .bit_packed()
            .expect("a three hundred wide range packs");
        assert_eq!(packed.form(), Form::BitPacked);
        let codes: Vec<u32> = (0..64).map(|row| ((row * 5) % 24) as u32).collect();
        let over = Vector::dictionary(codes, packed.clone()).expect("codes are in range");
        assert_eq!(over.form(), Form::Dictionary);
        for (name, returns) in [
            ("count", LogicalType::BigInt),
            ("sum", LogicalType::decimal(38, 2).expect("a legal decimal")),
            ("avg", LogicalType::Double),
            ("min", ty.clone()),
            ("max", ty.clone()),
        ] {
            agrees(name, &returns, std::slice::from_ref(&packed), name);
            agrees(name, &returns, std::slice::from_ref(&over), name);
        }
        assert_eq!(fallback::count(Kernel::Aggregate, Form::BitPacked, Form::BitPacked), 0);
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Dictionary, Form::Dictionary), 0);
        fallback::reset();
    }

    /// The same two forms scattered into groups, which is the half of this that q01 is.
    ///
    /// A grouped aggregate does not go through [`Accumulator::update_run`] at all, it goes through
    /// the scatter, so a packed column staying off the row at a time path there is a second thing
    /// to prove. The extremes are over both an integer column and a decimal one, since the scatter
    /// reads a decimal extreme on the unscaled integers now that one column holds one scale.
    #[test]
    fn a_packed_column_scattered_into_groups_does_not_reach_the_row_at_a_time_path() {
        let ty = LogicalType::decimal(15, 2).expect("a legal decimal");
        let values: Vec<Value> = (0..48)
            .map(|row| {
                if row % 7 == 0 {
                    Value::Null
                } else {
                    Value::Decimal { unscaled: 900 + row * 13, width: 15, scale: 2 }
                }
            })
            .collect();
        let packed = Vector::from_values(ty.clone(), &values)
            .expect("a flat decimal")
            .bit_packed()
            .expect("a six hundred wide range packs");
        assert_eq!(packed.form(), Form::BitPacked);
        let codes: Vec<u32> = (0..48).map(|row| ((row * 5) % 48) as u32).collect();
        let over = Vector::dictionary(codes, packed.clone()).expect("codes are in range");
        let whole = Vector::from_values(
            LogicalType::Integer,
            &(0..48).map(|row| Value::Integer(500 + (row * 7) % 29)).collect::<Vec<_>>(),
        )
        .expect("a flat integer column")
        .bit_packed()
        .expect("a range of twenty nine packs");
        assert_eq!(whole.form(), Form::BitPacked);
        let slots: Vec<usize> = (0..48).map(|row| row % 3).collect();
        for (name, returns, column) in [
            ("count", LogicalType::BigInt, &packed),
            ("sum", LogicalType::decimal(38, 2).expect("a legal decimal"), &packed),
            ("avg", LogicalType::Double, &packed),
            ("count", LogicalType::BigInt, &over),
            ("sum", LogicalType::decimal(38, 2).expect("a legal decimal"), &over),
            ("avg", LogicalType::Double, &over),
            ("min", LogicalType::Integer, &whole),
            ("max", LogicalType::Integer, &whole),
            ("min", ty.clone(), &packed),
            ("max", ty.clone(), &packed),
            ("min", ty.clone(), &over),
            ("max", ty.clone(), &over),
        ] {
            fallback::reset();
            let mut states = vec![Accumulator::new(name, &returns).expect("known"); 3];
            update_scattered(&mut states, &slots, 1, 0, Some(column), 48).expect("folds them in");
            let form = column.form();
            assert_eq!(
                fallback::count(Kernel::Aggregate, form, form),
                0,
                "{name} over a {form:?} column took the row at a time path"
            );
            // The groups against the same rows folded one accumulator at a time, so that staying
            // off the slow path is not on its own enough to pass.
            for (group, state) in states.iter().enumerate() {
                let mut one = Accumulator::new(name, &returns).expect("known");
                let mine: Vec<Value> = (0..48)
                    .filter(|row| slots[*row] == group)
                    .map(|row| column.try_value_at(row).expect("a value"))
                    .collect();
                for value in &mine {
                    one.update(std::slice::from_ref(value)).expect("folds one in");
                }
                assert_eq!(
                    state.clone().finish().expect("finishes"),
                    one.finish().expect("finishes"),
                    "{name} over group {group} of a {form:?} column"
                );
            }
        }
        fallback::reset();
    }

    /// A total of a column held as a hugeint, which is what a decimal wider than eighteen digits
    /// is, and which both totalling paths used to hand straight to the row at a time loop.
    ///
    /// q11 sums a `DECIMAL(34, 2)` column grouped and again ungrouped, and q09 sums a
    /// `DECIMAL(19, 4)` one grouped. Between them that was most of what was left in the fallback
    /// ledger of the twenty two TPC-H queries. The overflow case is here because it is the reason
    /// those paths declined the width in the first place: the answer to a total that does not fit
    /// is the error, and it has to still be the error.
    #[test]
    fn a_wide_decimal_totals_off_the_row_at_a_time_path_and_still_raises_on_overflow() {
        fallback::reset();
        let ty = LogicalType::decimal(34, 2).expect("a legal decimal");
        let sum = LogicalType::decimal(38, 2).expect("a legal decimal");
        let values: Vec<Value> = (0..40)
            .map(|row| {
                if row % 9 == 0 {
                    Value::Null
                } else {
                    // Past what sixty four bits hold, so the column is a hugeint and not a bigint
                    // that happens to be declared wide.
                    Value::Decimal {
                        unscaled: i128::from(row) * 1_000_000_000_000_000_000_000 + 7,
                        width: 34,
                        scale: 2,
                    }
                }
            })
            .collect();
        let column = Vector::from_values(ty.clone(), &values).expect("a flat wide decimal");
        for (name, returns) in [
            ("sum", sum.clone()),
            ("min", ty.clone()),
            ("max", ty.clone()),
            ("avg", LogicalType::Double),
        ] {
            agrees(name, &returns, std::slice::from_ref(&column), name);
        }
        // The same column into three groups, which is the shape q11 and q09 are in.
        let slots: Vec<usize> = (0..40).map(|row| row % 3).collect();
        let mut states = vec![Accumulator::new("sum", &sum).expect("known"); 3];
        update_scattered(&mut states, &slots, 1, 0, Some(&column), 40).expect("folds them in");
        for (group, state) in states.iter().enumerate() {
            let mut one = Accumulator::new("sum", &sum).expect("known");
            for row in (0..40).filter(|row| slots[*row] == group) {
                one.update(std::slice::from_ref(&values[row])).expect("folds one in");
            }
            assert_eq!(
                state.clone().finish().expect("finishes"),
                one.finish().expect("finishes"),
                "the sum of group {group}"
            );
        }
        assert_eq!(fallback::count(Kernel::Aggregate, Form::Flat, Form::Flat), 0);

        // Two values that cannot be added, ungrouped and then grouped, both of which have to
        // report rather than wrap.
        let huge = Value::Decimal { unscaled: i128::MAX - 1, width: 34, scale: 2 };
        let brims = Vector::from_values(ty.clone(), &[huge.clone(), huge.clone()]).expect("two");
        let mut ungrouped = Accumulator::new("sum", &sum).expect("known");
        let raised = ungrouped.update_run(std::slice::from_ref(&brims), 2);
        assert!(raised.is_err(), "a total that does not fit answered anyway");
        let mut grouped = vec![Accumulator::new("sum", &sum).expect("known"); 1];
        let one_group = vec![0_usize; 2];
        let scattered = update_scattered(&mut grouped, &one_group, 1, 0, Some(&brims), 2);
        assert!(scattered.is_err(), "a total that does not fit answered anyway");
        fallback::reset();
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

    /// Stands in for the text a native file keeps, which is reachable one value at a time and is
    /// not a run of bytes anything can take a slice of.
    #[derive(Debug)]
    struct Filed(Vec<Vec<u8>>);

    impl rudb_vector::TextSource for Filed {
        fn len(&self) -> usize {
            self.0.len()
        }

        fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
            Ok(self.0.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.0.iter().map(Vec::len).sum()
        }
    }

    /// A scan of a native text column hands over a dictionary whose payload is still in the file,
    /// so there is nothing for the gather to read and a minimum over one used to build a value per
    /// row. It is decided on the bytes, and the bytes can be asked for a code at a time.
    #[test]
    fn an_extreme_over_a_dictionary_that_keeps_its_bytes_in_a_file_is_decided_on_the_bytes() {
        let source = Arc::new(Filed(vec![b"pear".to_vec(), b"apple".to_vec(), b"plum".to_vec()]));
        let values = Vector::external_text(LogicalType::Varchar, source).expect("three values");
        let coded = Vector::dictionary(vec![0, 2, 1, 2, 0], values).expect("codes are in range");
        let batch = std::slice::from_ref(&coded);
        assert_eq!(
            a_vector_at_a_time("min", &LogicalType::Varchar, batch).expect("finds one"),
            Value::Varchar("apple".into())
        );
        assert_eq!(
            a_vector_at_a_time("max", &LogicalType::Varchar, batch).expect("finds one"),
            Value::Varchar("plum".into())
        );
        // The answers above are right either way, since falling back gets them too. These say the
        // vector path is the one that found them, and which row each of them won on.
        let smallest = gather(&coded, 5, &Validity::AllValid, Want::Extreme(true));
        assert!(matches!(smallest, Some(Contribution::Extreme(Some(2)))));
        let largest = gather(&coded, 5, &Validity::AllValid, Want::Extreme(false));
        assert!(matches!(largest, Some(Contribution::Extreme(Some(1)))));
    }

    /// The same thing [`Filed`] is, for a reader that also wrote down the order it sorted the
    /// values into, and which counts the reads so a test can say how many there were.
    #[derive(Debug)]
    struct Sorted {
        values: Vec<Vec<u8>>,
        order: Vec<u32>,
        ranked: std::sync::OnceLock<Option<Vec<u32>>>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl Sorted {
        fn over(words: &[&str]) -> Arc<Self> {
            let values: Vec<Vec<u8>> = words.iter().map(|word| word.as_bytes().to_vec()).collect();
            let mut order: Vec<u32> = (0..values.len() as u32).collect();
            order.sort_by(|&left, &right| values[left as usize].cmp(&values[right as usize]));
            Arc::new(Self {
                values,
                order,
                ranked: std::sync::OnceLock::new(),
                reads: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn reads(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl rudb_vector::TextSource for Sorted {
        fn len(&self) -> usize {
            self.values.len()
        }

        fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.values.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.values.iter().map(Vec::len).sum()
        }

        fn ranks(&self) -> Option<usize> {
            Some(self.order.len())
        }

        fn compare_rank(&self, rank: usize, wanted: &[u8]) -> Result<std::cmp::Ordering> {
            Ok(self.values[self.order[rank] as usize].as_slice().cmp(wanted))
        }

        fn code_at_rank(&self, rank: usize) -> Result<u32> {
            Ok(self.order[rank])
        }

        fn code_ranks(&self) -> Option<&[u32]> {
            self.ranked
                .get_or_init(|| {
                    let mut ranks = vec![0; self.order.len()];
                    for (rank, &code) in self.order.iter().enumerate() {
                        ranks[code as usize] = rank as u32;
                    }
                    Some(ranks)
                })
                .as_deref()
        }
    }

    /// An ungrouped min over a dictionary that knows its order holds the rank and reads nothing
    /// until it is finished, which is the whole difference between this and the byte path.
    ///
    /// The count is what the test is about. The byte path reduces a vector to its winning row and
    /// then reads the string there to hold it, so two vectors cost two reads and a hundred thousand
    /// cost a hundred thousand, each of which decodes a block of a payload and keeps it. One read
    /// here says the rank travelled instead and the value came out once at the end.
    #[test]
    fn an_ungrouped_extreme_over_a_sorted_dictionary_reads_one_value_however_many_vectors_it_saw() {
        let source = Sorted::over(&["pear", "apple", "plum", "fig"]);
        // One value vector behind every page, which is what a reader hands over and is what makes
        // two ranks from two vectors comparable at all.
        let handed: Arc<dyn rudb_vector::TextSource> = source.clone();
        let values =
            Arc::new(Vector::external_text(LogicalType::Varchar, handed).expect("four values"));
        // row at a time: each of these is its own vector, which is the point of the test.
        for (name, wanted) in [("min", "apple"), ("max", "plum")] {
            source.reads.store(0, std::sync::atomic::Ordering::Relaxed);
            let mut accumulator =
                Accumulator::new(name, &LogicalType::Varchar).expect("a known one");
            for codes in [vec![0_u32, 2, 3], vec![1, 0, 2], vec![3, 3, 0]] {
                let rows = codes.len();
                let coded = Vector::stable_dictionary(codes, Arc::clone(&values))
                    .expect("codes are in range");
                accumulator
                    .update_run(std::slice::from_ref(&coded), rows)
                    .expect("folds a vector in");
            }
            assert_eq!(
                accumulator.finish().expect("an extreme"),
                Value::Varchar(wanted.into()),
                "the {name} of three vectors"
            );
            assert_eq!(source.reads(), 1, "the {name} read the payload once");
        }
    }

    /// The flat sum reads the first `rows` values as one slice rather than one index at a time, so
    /// a vector that is asked for fewer rows than it holds has to stop where it was told rather
    /// than where the data ends.
    #[test]
    fn a_run_shorter_than_the_vector_totals_only_the_rows_it_was_asked_for() {
        let rows: Vec<Value> = (1..=10).map(Value::Integer).collect();
        let vector = Vector::from_values(LogicalType::Integer, &rows).expect("a vector");
        let batch = std::slice::from_ref(&vector);
        // row at a time: the point is that each count gives a different answer, so there is
        // nothing to batch.
        for count in 0..=10usize {
            let mut accumulator =
                Accumulator::new("sum", &LogicalType::HugeInt).expect("a known one");
            accumulator.update_run(batch, count).expect("totals");
            let wanted = (count * (count + 1) / 2) as i128;
            let got = accumulator.finish().expect("a total");
            if count == 0 {
                assert_eq!(got, Value::Null, "no rows is no total");
            } else {
                assert_eq!(got, Value::HugeInt(wanted), "the first {count} rows");
            }
        }
    }

    /// The padded copy and the plain gather are two ways to the same number, and which one runs
    /// depends on how wide the dictionary is. So the answer is held against the flat sum of the same
    /// rows at every width that matters: on each rung of the ladder, on the boundary between two of
    /// them, and past the last one where the copy is refused and the gather answers instead.
    #[test]
    fn a_dictionary_read_through_a_padded_copy_totals_what_the_same_rows_total_laid_out_flat() {
        // row at a time: each width is its own vector and its own expected answer.
        for distinct in [1usize, 2, 7, 255, TALLY_LIMIT, TALLY_LIMIT + 1, TALLY_LIMIT * 3] {
            let entries: Vec<Value> =
                (0..distinct).map(|slot| Value::Integer(slot as i32 * 7 - 11)).collect();
            let values = Vector::from_values(LogicalType::Integer, &entries).expect("a dictionary");
            // A stride that shares no factor with the rungs, so the codes walk the whole dictionary
            // rather than the first few entries of it.
            let codes: Vec<u32> = (0..1500u32).map(|row| row * 13 % distinct as u32).collect();
            let flat: Vec<Value> =
                codes.iter().map(|&code| entries[code as usize].clone()).collect();

            let coded = Vector::dictionary(codes, values).expect("codes are in range");
            let mut counted = Accumulator::new("sum", &LogicalType::HugeInt).expect("a known one");
            counted.update_run(std::slice::from_ref(&coded), 1500).expect("totals");

            let laid_out = Vector::from_values(LogicalType::Integer, &flat).expect("a vector");
            let mut gathered = Accumulator::new("sum", &LogicalType::HugeInt).expect("a known one");
            gathered.update_run(std::slice::from_ref(&laid_out), 1500).expect("totals");

            assert_eq!(
                counted.finish().expect("a total"),
                gathered.finish().expect("a total"),
                "a dictionary of {distinct} entries"
            );
        }
    }

    /// The copy is read for the rows it was asked for and not the ones past them, which the masking
    /// of the index would hide if the loop went over the whole code run.
    #[test]
    fn a_padded_dictionary_stops_at_the_rows_it_was_asked_for() {
        let entries = [Value::Integer(1), Value::Integer(100)];
        let values = Vector::from_values(LogicalType::Integer, &entries).expect("a dictionary");
        let coded = Vector::dictionary(vec![0, 0, 0, 1, 1], values).expect("codes are in range");
        let mut accumulator = Accumulator::new("sum", &LogicalType::HugeInt).expect("a known one");
        accumulator.update_run(std::slice::from_ref(&coded), 3).expect("totals");
        assert_eq!(
            accumulator.finish().expect("a total"),
            Value::HugeInt(3),
            "only the three ones"
        );
    }

    /// A mean over a decimal column is the exact total divided once.
    ///
    /// A decimal stores an integer and the type says where the point goes, so the column adds up
    /// exactly and the only rounding is the division at the end. Turning each row into a double and
    /// dividing it by a hundred before adding it rounds once per row instead, and those roundings do
    /// not cancel: the two answers below differ, and the first is the one duckdb gives.
    ///
    /// The same total whichever way the rows were divided is the other half of it. Two workers over
    /// one column reach an answer that does not depend on where the split fell, which a running
    /// double does not, and that is what makes a parallel `AVG` over a decimal repeatable.
    #[test]
    fn a_mean_of_a_decimal_column_adds_exactly_and_divides_once() {
        let ty = LogicalType::Decimal { width: 15, scale: 2 };
        let rows: Vec<Value> = (0..10_000)
            .map(|row: i64| Value::Decimal {
                unscaled: i128::from(row % 97 + 3),
                width: 15,
                scale: 2,
            })
            .collect();
        let total: i128 = (0..10_000i64).map(|row| i128::from(row % 97 + 3)).sum();
        #[expect(clippy::cast_precision_loss, reason = "the test is about which double comes out")]
        let exact = total as f64 / (rows.len() as f64 * 100.0);
        assert_eq!(run("avg", &LogicalType::Double, &rows), Value::Double(exact));

        // What the same column comes to when every row is descaled before it is added, which is
        // what this used to do. It has to differ, or the test above is asserting nothing.
        #[expect(clippy::cast_precision_loss, reason = "the test is about which double comes out")]
        let drifted = rows
            .iter()
            .map(|row| match row {
                Value::Decimal { unscaled, .. } => *unscaled as f64 / 100.0,
                other => unreachable!("{other:?}"),
            })
            .sum::<f64>()
            / rows.len() as f64;
        assert_ne!(drifted, exact, "the naive order has to round differently");

        let vector = Vector::from_values(ty, &rows).expect("a decimal vector");
        let mut whole = Accumulator::new("avg", &LogicalType::Double).expect("a known one");
        whole.update_run(std::slice::from_ref(&vector), rows.len()).expect("totals");
        assert_eq!(whole.finish().expect("finishes"), Value::Double(exact), "one vector");

        for cut in [1, 3_000, 9_999] {
            let mut first = Accumulator::new("avg", &LogicalType::Double).expect("a known one");
            let mut rest = Accumulator::new("avg", &LogicalType::Double).expect("a known one");
            for row in &rows[..cut] {
                first.update(std::slice::from_ref(row)).expect("accumulates");
            }
            for row in &rows[cut..] {
                rest.update(std::slice::from_ref(row)).expect("accumulates");
            }
            first.combine(&rest).expect("two halves of one column");
            assert_eq!(first.finish().expect("finishes"), Value::Double(exact), "split at {cut}");
        }
    }

    /// Why the whole sum checks every addition at a hundred and twenty eight bits: the total of
    /// one vector can overflow on its own there, and the overflow is the answer rather than a
    /// detail. The vector path hands a total that does not fit back rather than reporting it
    /// itself, so the error comes from the same row at a time loop it always came from.
    #[test]
    fn a_total_of_hugeints_that_does_not_fit_goes_the_row_at_a_time_way_and_overflows() {
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
