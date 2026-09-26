//! The aggregates that are not a count, a total or an extreme.
//!
//! [`crate::aggregate::Accumulator`] keeps `count`, `sum`, `avg`, `min` and `max` in small inline
//! states because a grouped query holds one per group per call and those five are most of what
//! grouped queries ask for. Everything else lands here, behind one box, so that adding `list` or
//! `stddev` does not make every `count(*)` state in a hash table wider than it is now.
//!
//! These states fold values and not vectors. The vector paths in the aggregate module step aside
//! for them and hand over one row at a time, which is correct for all of them and fast for none,
//! and each one that turns out to matter can grow a vector path of its own later.
//!
//! Not every aggregate here skips nulls. `list` keeps them as elements and `first` and `last` can
//! answer with one, which is the pin's behavior and is why the null check lives in each arm rather
//! than in front of all of them.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, Value};

use crate::aggregate::Accumulator;
use crate::arg_extreme::{ArgExtreme, Key};
use crate::bitstring::Gathered;
use crate::compare::order_with_nulls;
use crate::hash::Sketch;
use crate::histogram::Binned;
use crate::number::{approximate, fit, integral};
use crate::quantile::{self, Column, Held, Holistic};
use crate::statistics::{Moment, Paired, Pairing, Powers};
use crate::tally::Tally;

/// A running aggregate that is not one of the five the aggregate module keeps inline.
#[derive(Debug, Clone)]
pub(crate) enum General {
    /// `list` and `array_agg`: every value in the order it arrived, nulls included.
    List { element: LogicalType, values: Vec<Value> },
    /// `first`, `last` and `any_value`, which keep one row's value.
    Pick { held: Option<Value>, pick: Pick },
    /// `bool_and` and `bool_or`.
    Logic { held: Option<bool>, all: bool },
    /// `bit_and`, `bit_or` and `bit_xor`, held wide and narrowed to the argument's type at the end.
    Bits { held: Option<i128>, op: BitOp, returns: LogicalType },
    /// `bit_and`, `bit_or` and `bit_xor` over bit strings, which all have to be one length.
    BitString { held: Option<Vec<u8>>, op: BitOp },
    /// `bitstring_agg`, in [`crate::bitstring`].
    Gathered(Gathered),
    /// `approx_count_distinct`, in [`crate::hash`].
    Sketched(Sketch),
    /// `product`, in floating point the way the pin multiplies.
    Product { total: f64, seen: bool },
    /// The variance family, as a running count, mean and sum of squared differences.
    ///
    /// This is Welford's update and the pin's combine, step for step, because the order the
    /// arithmetic happens in is the last digit of the answer.
    Moments { count: u64, mean: f64, squared: f64, measure: Measure },
    /// `string_agg`, the text so far, whether anything has gone into it, and the separator it was
    /// joined with, which a combine has to put between the two halves.
    Joined { text: String, seen: bool, separator: String },
    /// An aggregate whose call said which order to read its rows in, `list(x ORDER BY y)`.
    ///
    /// Every row is held as it came, the aggregate's arguments followed by the sort keys, and the
    /// rows are sorted and handed to a fresh copy of `inner` only when the answer is asked for. A
    /// combine is then two runs of rows put together, which is why the order the threads finish in
    /// does not reach the answer. The sort is stable, so rows that tie on every key keep the order
    /// they arrived in, which is the most the pin promises too.
    Ordered { keys: Vec<(bool, bool)>, rows: Vec<Vec<Value>>, inner: Box<Accumulator> },
    /// The quantiles, `median`, `mad` and `mode`, which hold every value that is not null and the
    /// fraction the call asked for, and answer in [`crate::quantile`].
    Holistic { values: Held, fraction: Option<Value>, measure: Holistic, returns: LogicalType },
    /// The `arg_min` and `arg_max` spellings, which keep the row with the least or greatest key,
    /// or the best `n` of them when the call passes a count, and answer in [`crate::arg_extreme`].
    Arg { state: ArgExtreme, returns: LogicalType },
    /// `corr`, the covariances and the `regr_*` family, over pairs, in [`crate::statistics`].
    Paired(Paired),
    /// `skewness`, `kurtosis` and `kurtosis_pop`, in [`crate::statistics`].
    Powers(Powers),
    /// `fsum` and `favg`, a sum with Kahan's running error, in the pin's steps.
    Kahan { value: f64, err: f64, count: u64, average: bool },
    /// `count_if`, the rows that were true, and whether any row was not null.
    CountIf { count: i128, seen: bool },
    /// `entropy`, which counts each distinct value rather than holding them all.
    Tally(Tally),
    /// `histogram(x)`, the count of each distinct value, answered as a map keyed by `key`.
    ///
    /// A call that passes bins becomes [`General::Binned`] at its first row, since the name alone
    /// does not say which of the two it is.
    Counted { tally: Tally, key: LogicalType },
    /// `histogram(x, bins)` and `histogram_exact(x, bins)`, in [`crate::histogram`].
    Binned(Binned),
}

/// Which row [`General::Pick`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    /// The first row, even when it is null.
    First,
    /// The last row, even when it is null.
    Last,
    /// The first row that is not null.
    Any,
}

/// Which bitwise fold [`General::Bits`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitOp {
    And,
    Or,
    Xor,
}

/// Which answer [`General::Moments`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Measure {
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
    /// `sem`, the standard error of the mean.
    Sem,
}

impl General {
    /// A fresh state for `name`, or `None` when the name is not one of these.
    pub(crate) fn new(name: &str, returns: &LogicalType) -> Option<Self> {
        let pick = |pick| Self::Pick { held: None, pick };
        let bits = |op| {
            if *returns == LogicalType::Bit {
                Self::BitString { held: None, op }
            } else {
                Self::Bits { held: None, op, returns: returns.clone() }
            }
        };
        let moments = |measure| Self::Moments { count: 0, mean: 0.0, squared: 0.0, measure };
        if let Some(state) = ArgExtreme::named(name) {
            return Some(Self::Arg { state, returns: returns.clone() });
        }
        if let Some(measure) = Pairing::named(name) {
            return Some(Self::Paired(Paired::new(measure)));
        }
        if let Some(measure) = Moment::named(name) {
            return Some(Self::Powers(Powers::new(measure)));
        }
        if let Some(measure) = Holistic::named(name) {
            let returns = returns.clone();
            return Some(Self::Holistic { values: Held::Empty, fraction: None, measure, returns });
        }
        Some(match name {
            "list" => {
                let element = match returns {
                    LogicalType::List(element) => (**element).clone(),
                    _ => LogicalType::Null,
                };
                Self::List { element, values: Vec::new() }
            }
            "first" => pick(Pick::First),
            "last" => pick(Pick::Last),
            "any_value" => pick(Pick::Any),
            "bool_and" => Self::Logic { held: None, all: true },
            "bool_or" => Self::Logic { held: None, all: false },
            "bit_and" => bits(BitOp::And),
            "bit_or" => bits(BitOp::Or),
            "bit_xor" => bits(BitOp::Xor),
            "bitstring_agg" => Self::Gathered(Gathered::default()),
            "approx_count_distinct" => Self::Sketched(Sketch::default()),
            "product" => Self::Product { total: 1.0, seen: false },
            "var_samp" => moments(Measure::VarSamp),
            "var_pop" => moments(Measure::VarPop),
            "stddev_samp" => moments(Measure::StddevSamp),
            "stddev_pop" => moments(Measure::StddevPop),
            "sem" => moments(Measure::Sem),
            "fsum" => Self::Kahan { value: 0.0, err: 0.0, count: 0, average: false },
            "favg" => Self::Kahan { value: 0.0, err: 0.0, count: 0, average: true },
            "count_if" => Self::CountIf { count: 0, seen: false },
            "entropy" => Self::Tally(Tally::Empty),
            "histogram" => Self::Counted { tally: Tally::Empty, key: map_key(returns) },
            "histogram_exact" => Self::Binned(Binned::new(true, map_key(returns))),
            "string_agg" => {
                Self::Joined { text: String::new(), seen: false, separator: String::new() }
            }
            _ => return None,
        })
    }

    /// Folds one row in. `args` is the row's arguments in call order.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        let Some(value) = args.first() else {
            return Err(Error::internal("an aggregate over 0 arguments".to_string()));
        };
        match self {
            Self::Arg { state, .. } => state.update(args)?,
            Self::List { values, .. } => values.push(value.clone()),
            Self::Pick { held, pick } => match pick {
                Pick::First => {
                    if held.is_none() {
                        *held = Some(value.clone());
                    }
                }
                Pick::Last => *held = Some(value.clone()),
                Pick::Any => {
                    if held.is_none() && !value.is_null() {
                        *held = Some(value.clone());
                    }
                }
            },
            // The rows of an ordered call are kept whole, nulls and all, and the aggregate it wraps
            // decides what to skip once they are in order.
            Self::Ordered { rows, .. } => rows.push(args.to_vec()),
            Self::Paired(state) => state.update(args)?,
            _ if value.is_null() => {}
            Self::Counted { key, .. } if args.len() > 1 => {
                let mut binned = Binned::new(false, key.clone());
                binned.update(value, args.get(1))?;
                *self = Self::Binned(binned);
            }
            Self::Counted { tally, .. } => tally.push(value)?,
            Self::Binned(state) => state.update(value, args.get(1))?,
            Self::Powers(state) => {
                state.add(approximate(value).ok_or_else(|| unexpected("skewness", value))?);
            }
            Self::Holistic { values, fraction, .. } => {
                if fraction.is_none() {
                    *fraction = args.get(1).cloned();
                }
                values.push(value);
            }
            Self::Logic { held, all } => {
                let Value::Boolean(flag) = *value else {
                    return Err(unexpected("bool_and", value));
                };
                let so_far = held.unwrap_or(*all);
                *held = Some(if *all { so_far && flag } else { so_far || flag });
            }
            Self::Bits { held, op, .. } => {
                let bits = integral(value).ok_or_else(|| unexpected("bit_and", value))?;
                *held = Some(match (*held, *op) {
                    (None, _) => bits,
                    (Some(so_far), BitOp::And) => so_far & bits,
                    (Some(so_far), BitOp::Or) => so_far | bits,
                    (Some(so_far), BitOp::Xor) => so_far ^ bits,
                });
            }
            Self::BitString { held, op } => {
                let Value::Bit(bits) = value else { return Err(unexpected("bit_and", value)) };
                match held {
                    None => *held = Some(bits.clone()),
                    Some(so_far) => fold_bits(so_far, bits, *op)?,
                }
            }
            Self::Gathered(state) => state.update(value, &args[1..])?,
            Self::Sketched(sketch) => sketch.insert(value),
            Self::Product { total, seen } => {
                *total *= approximate(value).ok_or_else(|| unexpected("product", value))?;
                *seen = true;
            }
            Self::Kahan { value: summed, err, count, .. } => {
                kahan(approximate(value).ok_or_else(|| unexpected("fsum", value))?, summed, err);
                *count += 1;
            }
            Self::Tally(tally) => tally.push(value)?,
            Self::CountIf { count, seen } => {
                let Value::Boolean(flag) = *value else {
                    return Err(unexpected("count_if", value));
                };
                *count += i128::from(flag);
                *seen = true;
            }
            Self::Moments { count, mean, squared, .. } => {
                let input = approximate(value).ok_or_else(|| unexpected("stddev", value))?;
                *count += 1;
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let differential = (input - *mean) / *count as f64;
                let next = *mean + differential;
                *squared += (input - next) * (input - *mean);
                *mean = next;
            }
            Self::Joined { text, seen, separator: kept } => {
                // A null separator drops the row, which is how the pin answers
                // `string_agg(x, NULL)` with null.
                let separator = match args.get(1) {
                    None => ",",
                    Some(Value::Varchar(separator)) => separator,
                    Some(_) => return Ok(()),
                };
                let Value::Varchar(piece) = value else {
                    return Err(unexpected("string_agg", value));
                };
                if *seen {
                    text.push_str(separator);
                } else {
                    separator.clone_into(kept);
                }
                text.push_str(piece);
                *seen = true;
            }
        }
        Ok(())
    }

    /// Whether this state can say from a typed `by` alone that a row changes nothing, through
    /// [`Self::cannot_take`].
    pub(crate) fn keyed(&self) -> bool {
        matches!(self, Self::Arg { .. })
    }

    /// Whether a row whose second argument is `key` is sure to change nothing, for a call of
    /// `arity` arguments.
    pub(crate) fn cannot_take(&self, key: Key, arity: usize) -> bool {
        matches!(self, Self::Arg { state, .. } if state.cannot_take(key, arity))
    }

    /// Whether this state skips nulls and takes the rest of a column through [`Self::push_column`].
    pub(crate) fn takes_columns(&self) -> bool {
        matches!(
            self,
            Self::Holistic { .. }
                | Self::Tally(_)
                | Self::Kahan { .. }
                | Self::CountIf { .. }
                | Self::Counted { .. }
                | Self::Binned(_)
        )
    }

    /// Adds the row of a column a [`Column`] reads, with the fraction read off `args` the first
    /// time. Nothing happens for a state that [`Self::takes_columns`] says no to.
    pub(crate) fn push_column(
        &mut self,
        column: Column<'_>,
        row: usize,
        args: &[rudb_vector::Vector],
    ) -> Result<()> {
        if let Self::Binned(state) = self
            && state.push_column(column, row)
        {
            return Ok(());
        }
        match (&mut *self, column) {
            (Self::Tally(tally), column) => return tally.push_column(column, row),
            (Self::Counted { tally, .. }, column) if args.len() < 2 => {
                return tally.push_column(column, row);
            }
            (Self::Counted { .. } | Self::Binned(_), column) => {
                let mut row_args = vec![column.value(row)];
                for arg in args.iter().skip(1) {
                    row_args.push(arg.try_value_at(row)?);
                }
                return self.update(&row_args);
            }
            (Self::CountIf { count, seen }, Column::Flags(flags)) => {
                *count += i128::from(flags[row]);
                *seen = true;
                return Ok(());
            }
            (Self::Kahan { value, err, count, .. }, Column::Reals(reals)) => {
                kahan(reals[row], value, err);
                *count += 1;
                return Ok(());
            }
            (Self::CountIf { .. } | Self::Kahan { .. }, column) => {
                return self.update(&[column.value(row)]);
            }
            _ => {}
        }
        let Self::Holistic { values, fraction, .. } = self else {
            return Ok(());
        };
        if fraction.is_none()
            && let Some(given) = args.get(1)
        {
            *fraction = Some(given.try_value_at(row)?);
        }
        column.push(values, row);
        Ok(())
    }

    /// Folds another state for the same call into this one, as if its rows came after these.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (self, other) {
            (Self::List { values, .. }, Self::List { values: more, .. }) => {
                values.extend(more.iter().cloned());
            }
            (Self::Arg { state, .. }, Self::Arg { state: theirs, .. }) => state.combine(theirs)?,
            (Self::Ordered { rows, .. }, Self::Ordered { rows: more, .. }) => {
                rows.extend(more.iter().cloned());
            }
            (
                Self::Holistic { values, fraction, .. },
                Self::Holistic { values: more, fraction: theirs, .. },
            ) => {
                values.append(more);
                if fraction.is_none() {
                    fraction.clone_from(theirs);
                }
            }
            (Self::Pick { held, pick }, Self::Pick { held: theirs, .. }) => {
                let take = match pick {
                    Pick::First | Pick::Any => held.is_none(),
                    Pick::Last => theirs.is_some(),
                };
                if take {
                    held.clone_from(theirs);
                }
            }
            (Self::Logic { held, all }, Self::Logic { held: theirs, .. }) => {
                *held = match (*held, *theirs) {
                    (Some(here), Some(there)) => {
                        Some(if *all { here && there } else { here || there })
                    }
                    (here, there) => here.or(there),
                };
            }
            (Self::Bits { held, op, .. }, Self::Bits { held: theirs, .. }) => {
                *held = match (*held, *theirs) {
                    (Some(here), Some(there)) => Some(match op {
                        BitOp::And => here & there,
                        BitOp::Or => here | there,
                        BitOp::Xor => here ^ there,
                    }),
                    (here, there) => here.or(there),
                };
            }
            (Self::BitString { held, op }, Self::BitString { held: theirs, .. }) => {
                match (held.as_mut(), theirs) {
                    (Some(here), Some(there)) => fold_bits(here, there, *op)?,
                    (None, there) => held.clone_from(there),
                    (Some(_), None) => {}
                }
            }
            (Self::Gathered(state), Self::Gathered(theirs)) => state.combine(theirs),
            (Self::Sketched(sketch), Self::Sketched(theirs)) => sketch.combine(theirs),
            (Self::Paired(state), Self::Paired(theirs)) => state.combine(theirs),
            (Self::Powers(state), Self::Powers(theirs)) => state.combine(theirs),
            (
                Self::Kahan { value, err, count, .. },
                Self::Kahan { value: theirs, err: their_err, count: more, .. },
            ) => {
                kahan(*theirs, value, err);
                kahan(*their_err, value, err);
                *count += more;
            }
            (Self::Tally(tally), Self::Tally(theirs)) => tally.append(theirs)?,
            (Self::Counted { tally, .. }, Self::Counted { tally: theirs, .. }) => {
                tally.append(theirs)?;
            }
            (Self::Binned(state), Self::Binned(theirs)) => state.combine(theirs)?,
            // A group that saw no rows has not learned it was binned yet.
            (Self::Binned(_), Self::Counted { .. }) => {}
            (here @ Self::Counted { .. }, Self::Binned(theirs)) => {
                *here = Self::Binned(theirs.clone());
            }
            (Self::CountIf { count, seen }, Self::CountIf { count: more, seen: any }) => {
                *count += more;
                *seen |= any;
            }
            (Self::Product { total, seen }, Self::Product { total: theirs, seen: any }) => {
                *total *= theirs;
                *seen |= any;
            }
            (
                Self::Moments { count, mean, squared, .. },
                Self::Moments { count: more, mean: theirs, squared: their_squared, .. },
            ) => {
                if *count == 0 {
                    (*count, *mean, *squared) = (*more, *theirs, *their_squared);
                } else if *more > 0 {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "the count of rows in one group is well inside the exact range"
                    )]
                    let (here, there) = (*count as f64, *more as f64);
                    let total = here + there;
                    let delta = theirs - *mean;
                    *squared = their_squared + *squared + delta * delta * there * here / total;
                    // The pin moves the mean with a fused multiply and add, which rounds once.
                    *mean = (there / total).mul_add(delta, *mean);
                    *count += more;
                }
            }
            (
                Self::Joined { text, seen, separator },
                Self::Joined { text: theirs, seen: any, separator: their_separator },
            ) => {
                if !*seen {
                    text.clone_from(theirs);
                    separator.clone_from(their_separator);
                    *seen = *any;
                } else if *any {
                    text.push_str(separator);
                    text.push_str(theirs);
                }
            }
            (here, there) => {
                return Err(Error::internal(format!(
                    "combining a {here:?} aggregate state with a {there:?} one"
                )));
            }
        }
        Ok(())
    }

    /// The answer of an aggregate with no `GROUP BY`. The pin combines its one state into an empty
    /// one before finishing it and a grouped state is finished as it stands, which only `fsum` and
    /// `favg` can tell apart.
    pub(crate) fn finish_ungrouped(&self) -> Result<Value> {
        let Self::Kahan { value, err, count, average } = self else { return self.finish() };
        let (value, err) = settled(*value, *err);
        Self::Kahan { value, err, count: *count, average: *average }.finish()
    }

    /// The answer.
    pub(crate) fn finish(&self) -> Result<Value> {
        if let Self::Ordered { keys, rows, inner } = self {
            return ordered(keys, rows, inner);
        }
        if let Self::Arg { state, returns } = self {
            return state.finish(returns);
        }
        if let Self::Holistic { values, fraction, measure, returns } = self {
            return quantile::finish(*measure, values, fraction.as_ref(), returns);
        }
        Ok(match self {
            Self::List { values, .. } if values.is_empty() => Value::Null,
            Self::List { element, values } => {
                Value::List { element: element.clone(), values: values.clone() }
            }
            Self::Pick { held, .. } => held.clone().unwrap_or(Value::Null),
            Self::Logic { held, .. } => held.map_or(Value::Null, Value::Boolean),
            Self::Bits { held: None, .. } | Self::Product { seen: false, .. } => Value::Null,
            Self::Bits { held: Some(bits), returns, .. } => {
                let returns =
                    if *returns == LogicalType::Null { &LogicalType::BigInt } else { returns };
                fit(*bits, returns).ok_or_else(|| {
                    Error::out_of_range(format!(
                        "a bitwise aggregate of {bits} does not fit in {returns}"
                    ))
                })?
            }
            Self::Product { total, .. } => Value::Double(*total),
            Self::Kahan { count: 0, .. } | Self::CountIf { seen: false, .. } => Value::Null,
            Self::Kahan { value, average: false, .. } => Value::Double(*value),
            Self::Kahan { value, err, count, average: true } => {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the pin divides by the count the same way"
                )]
                let rows = *count as f64;
                Value::Double(value / rows + err / rows)
            }
            Self::CountIf { count, .. } => Value::HugeInt(*count),
            Self::Tally(tally) => tally.entropy()?,
            Self::Counted { tally, key } => {
                let entries = tally.sorted()?;
                if entries.is_empty() {
                    return Ok(Value::Null);
                }
                let entries =
                    entries.into_iter().map(|(value, count)| (value, Value::UBigInt(count)));
                Value::map(key.clone(), LogicalType::UBigInt, entries.collect())
            }
            Self::Binned(state) => state.finish(),
            Self::BitString { held, .. } => held.clone().map_or(Value::Null, Value::Bit),
            Self::Gathered(state) => state.finish(),
            Self::Sketched(sketch) => Value::BigInt(sketch.count()),
            Self::Moments { count, squared, measure, .. } => {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "the count of rows in one group is well inside the exact range"
                )]
                let rows = *count as f64;
                if *measure == Measure::Sem {
                    return Ok(if *count == 0 {
                        Value::Null
                    } else {
                        Value::Double((squared / rows).sqrt() / rows.sqrt())
                    });
                }
                let sample = matches!(measure, Measure::VarSamp | Measure::StddevSamp);
                let variance = match (*count, sample) {
                    (0, _) | (1, true) => return Ok(Value::Null),
                    (1, false) => 0.0,
                    (_, true) => squared / (rows - 1.0),
                    (_, false) => squared / rows,
                };
                match measure {
                    Measure::VarSamp | Measure::VarPop => Value::Double(variance),
                    Measure::StddevSamp | Measure::StddevPop | Measure::Sem => {
                        Value::Double(variance.sqrt())
                    }
                }
            }
            Self::Paired(state) => state.finish(),
            Self::Powers(state) => state.finish()?,
            Self::Joined { seen: false, .. } => Value::Null,
            Self::Joined { text, .. } => Value::Varchar(text.clone()),
            Self::Ordered { .. } | Self::Holistic { .. } | Self::Arg { .. } => {
                return Err(Error::internal("an ordered, holistic or arg_min aggregate"));
            }
        })
    }
}

/// The key type of a map an aggregate answers with, or the null type when it does not answer one.
fn map_key(returns: &LogicalType) -> LogicalType {
    match returns {
        LogicalType::Map(key, _) => (**key).clone(),
        _ => LogicalType::Null,
    }
}

/// The answer of an ordered aggregate: its rows sorted on the keys at their end and folded into a
/// fresh copy of the aggregate in that order.
fn ordered(keys: &[(bool, bool)], rows: &[Vec<Value>], inner: &Accumulator) -> Result<Value> {
    let mut failure = None;
    let mut sorted: Vec<&Vec<Value>> = rows.iter().collect();
    sorted.sort_by(|left, right| {
        let (left, right) = (&left[left.len() - keys.len()..], &right[right.len() - keys.len()..]);
        for ((a, b), &(descending, nulls_first)) in left.iter().zip(right).zip(keys) {
            // Nulls are placed before the direction is applied, so `DESC NULLS LAST` still puts
            // them last.
            let placed = match order_with_nulls(a, b, nulls_first) {
                Ok(ordering) if descending && !a.is_null() && !b.is_null() => ordering.reverse(),
                Ok(ordering) => ordering,
                Err(error) => {
                    failure.get_or_insert(error);
                    Ordering::Equal
                }
            };
            if placed != Ordering::Equal {
                return placed;
            }
        }
        Ordering::Equal
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let mut fresh = inner.clone();
    for row in sorted {
        fresh.update(&row[..row.len() - keys.len()])?;
    }
    fresh.finish()
}

/// One step of the pin's Kahan sum. The pin's `fsum` answers with the sum alone and never adds
/// the error back in, so the error only matters for the next step.
fn kahan(input: f64, summed: &mut f64, err: &mut f64) {
    let diff = input - *err;
    let next = *summed + diff;
    *err = (next - *summed) - diff;
    *summed = next;
}

/// A Kahan state combined into an empty one the way the pin does it, which adds the running error
/// in as if it were a value, so an overflowing sum ends as NaN rather than infinity and a finite
/// one picks up its error.
fn settled(value: f64, err: f64) -> (f64, f64) {
    let (mut summed, mut next_err) = (0.0, 0.0);
    kahan(value, &mut summed, &mut next_err);
    kahan(err, &mut summed, &mut next_err);
    (summed, next_err)
}

/// A value of a type the binder should not have let through to this aggregate.
fn unexpected(name: &str, value: &Value) -> Error {
    Error::internal(format!("{name} was handed a {}", value.logical_type()))
}

/// Folds a bit string into the one held so far, which has to be as long, with the pin's error when
/// it is not.
fn fold_bits(held: &mut [u8], bits: &[u8], op: BitOp) -> Result<()> {
    if rudb_common::bit::len(held) != rudb_common::bit::len(bits) {
        let what = match op {
            BitOp::And => "AND",
            BitOp::Or => "OR",
            BitOp::Xor => "XOR",
        };
        return Err(Error::invalid_input(format!("Cannot {what} bit strings of different sizes")));
    }
    for (byte, theirs) in held[1..].iter_mut().zip(&bits[1..]) {
        *byte = match op {
            BitOp::And => *byte & theirs,
            BitOp::Or => *byte | theirs,
            BitOp::Xor => *byte ^ theirs,
        };
    }
    rudb_common::bit::finalize(held);
    Ok(())
}
