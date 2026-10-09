//! PostgreSQL's ordered-set and hypothetical-set aggregates, as `orderedsetaggs.c` computes them.
//!
//! `percentile_cont`, `percentile_disc` and `mode` read the values of their one `WITHIN GROUP` key
//! in the order of that key, and `rank`, `dense_rank`, `percent_rank` and `cume_dist` place a
//! hypothetical row, made of their direct arguments, among the rows of the group. The binder gives
//! a PostgreSQL session the names of PostgreSQL's final functions, made into an ordered name by
//! [`crate::ordered_name`] so that the sort keys come with them.
//!
//! A percentile is picked the way PostgreSQL picks it. `percentile_disc` is the row
//! `ceil(p * n)`, the first row for a `p` of zero. `percentile_cont` reads the rows
//! `floor(p * (n - 1))` and `ceil(p * (n - 1))` and goes between them as `lo + (hi - lo) * d`,
//! which is not the arithmetic of the pin's `quantile_cont`, so the last digit can differ. The
//! fraction is the direct argument of the first row of the group, which is one value for a group,
//! since the binder lets a direct argument read grouped columns only. A null fraction answers a
//! null, and an array of fractions answers an array of the same shape.
//!
//! The hypothetical row goes before its peers for `rank`, `dense_rank` and `percent_rank` and
//! after them for `cume_dist`, so the rows only have to be counted, not sorted. Only `dense_rank`
//! keeps rows, the ones that sort before the hypothetical row, to count how many of them differ.

use std::cmp::Ordering;

use rudb_common::{Error, LogicalType, Result, SqlState, StateKey, Value};

use crate::compare::{float_order, order, order_with_nulls};
use crate::datetime::{combine, scaled};
use crate::number::approximate;
use crate::printf::printf;
use crate::quantile::Held;

/// What a PostgreSQL ordered-set call answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Measure {
    Continuous,
    Discrete,
    Mode,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
}

impl Measure {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "percentile_cont_final" => Self::Continuous,
            "percentile_disc_final" => Self::Discrete,
            "mode_final" => Self::Mode,
            "hypothetical_rank_final" => Self::Rank,
            "hypothetical_dense_rank_final" => Self::DenseRank,
            "hypothetical_percent_rank_final" => Self::PercentRank,
            "hypothetical_cume_dist_final" => Self::CumeDist,
            _ => return None,
        })
    }
}

/// The state of one PostgreSQL ordered-set call.
#[derive(Debug, Clone)]
pub(crate) enum OrderedSet {
    /// `percentile_cont`, `percentile_disc` and `mode`, which hold the values that are not null.
    Sorted {
        measure: Measure,
        descending: bool,
        values: Held,
        /// The direct argument of the first row, for a percentile.
        fraction: Option<Value>,
        returns: LogicalType,
    },
    /// The hypothetical-set calls, which count the rows on each side of the hypothetical row.
    Hypothetical(Box<Hypothetical>),
}

/// The counts of a hypothetical-set call. A row is its keys followed by the direct arguments.
#[derive(Debug, Clone)]
pub(crate) struct Hypothetical {
    measure: Measure,
    keys: Vec<StateKey>,
    /// The hypothetical row, the direct arguments of the first row.
    direct: Option<Vec<Value>>,
    rows: i64,
    before: i64,
    peers: i64,
    /// The rows that sort before the hypothetical row, for `dense_rank` alone.
    lesser: Vec<Vec<Value>>,
}

impl OrderedSet {
    /// A fresh state for the ordered call `name` with the sort keys `keys`, or `None` when the name
    /// is not one of these.
    pub(crate) fn named(name: &str, keys: &[StateKey], returns: &LogicalType) -> Option<Self> {
        let measure = Measure::named(name)?;
        if matches!(measure, Measure::Continuous | Measure::Discrete | Measure::Mode) {
            let [key] = keys else { return None };
            return Some(Self::Sorted {
                measure,
                descending: key.descending,
                values: Held::Empty,
                fraction: None,
                returns: returns.clone(),
            });
        }
        Some(Self::Hypothetical(Box::new(Hypothetical {
            measure,
            keys: keys.to_vec(),
            direct: None,
            rows: 0,
            before: 0,
            peers: 0,
            lesser: Vec::new(),
        })))
    }

    /// Folds one row in, nulls included.
    pub(crate) fn update(&mut self, args: &[Value]) -> Result<()> {
        match self {
            Self::Sorted { values, fraction, .. } => {
                if fraction.is_none()
                    && let Some(given) = args.get(1)
                {
                    *fraction = Some(given.clone());
                }
                match args.first() {
                    Some(value) if !value.is_null() => values.push(value),
                    _ => {}
                }
                Ok(())
            }
            Self::Hypothetical(state) => state.update(args),
        }
    }

    /// Folds another state for the same call into this one.
    pub(crate) fn combine(&mut self, other: &Self) -> Result<()> {
        match (self, other) {
            (
                Self::Sorted { values, fraction, .. },
                Self::Sorted { values: more, fraction: theirs, .. },
            ) => {
                values.append(more);
                if fraction.is_none() {
                    fraction.clone_from(theirs);
                }
            }
            (Self::Hypothetical(state), Self::Hypothetical(theirs)) => {
                if state.direct.is_none() {
                    state.direct.clone_from(&theirs.direct);
                }
                state.rows += theirs.rows;
                state.before += theirs.before;
                state.peers += theirs.peers;
                state.lesser.extend(theirs.lesser.iter().cloned());
            }
            _ => return Err(Error::internal("two different ordered-set states to combine")),
        }
        Ok(())
    }

    /// The answer.
    pub(crate) fn finish(&self) -> Result<Value> {
        match self {
            Self::Sorted { measure: Measure::Mode, descending, values, .. } => {
                mode(&sorted(values, *descending)?)
            }
            Self::Sorted { measure, descending, values, fraction, returns } => {
                let continuous = *measure == Measure::Continuous;
                let Some(fraction) = fraction else { return Ok(Value::Null) };
                if let Value::List { .. } = fraction {
                    // An array of fractions answers a null for a group with no values before it
                    // looks at the array, as `percentile_disc_multi_final` does.
                    if values.len() == 0 {
                        return Ok(Value::Null);
                    }
                    checked(fraction)?;
                    let values = sorted(values, *descending)?;
                    return picked(fraction, &values, continuous, returns);
                }
                if fraction.is_null() {
                    return Ok(Value::Null);
                }
                let share = share(fraction)?;
                if values.len() == 0 {
                    return Ok(Value::Null);
                }
                percentile(&sorted(values, *descending)?, share, continuous)
            }
            Self::Hypothetical(state) => state.finish(),
        }
    }
}

impl Hypothetical {
    fn update(&mut self, args: &[Value]) -> Result<()> {
        let width = self.keys.len();
        if args.len() != 2 * width {
            return Err(Error::internal(format!(
                "a hypothetical-set call over {} arguments and {width} keys",
                args.len()
            )));
        }
        let (row, direct) = args.split_at(width);
        let direct = self.direct.get_or_insert_with(|| direct.to_vec());
        self.rows += 1;
        match placed(&self.keys, row, direct)? {
            Ordering::Less => {
                self.before += 1;
                if self.measure == Measure::DenseRank {
                    self.lesser.push(row.to_vec());
                }
            }
            Ordering::Equal => self.peers += 1,
            Ordering::Greater => {}
        }
        Ok(())
    }

    #[expect(clippy::cast_precision_loss, reason = "PostgreSQL divides the counts as doubles too")]
    fn finish(&self) -> Result<Value> {
        Ok(match self.measure {
            Measure::Rank => Value::BigInt(self.before + 1),
            Measure::DenseRank => {
                let mut rows: Vec<&Vec<Value>> = self.lesser.iter().collect();
                let mut failure = None;
                let mut compare = |left: &&Vec<Value>, right: &&Vec<Value>| {
                    placed(&self.keys, left, right).unwrap_or_else(|error| {
                        failure.get_or_insert(error);
                        Ordering::Equal
                    })
                };
                rows.sort_by(&mut compare);
                let mut distinct = i64::from(!rows.is_empty());
                for pair in rows.windows(2) {
                    if compare(&pair[0], &pair[1]) != Ordering::Equal {
                        distinct += 1;
                    }
                }
                if let Some(error) = failure {
                    return Err(error);
                }
                Value::BigInt(distinct + 1)
            }
            Measure::PercentRank if self.rows == 0 => Value::Double(0.0),
            Measure::PercentRank => Value::Double(self.before as f64 / self.rows as f64),
            _ => Value::Double((self.before + self.peers + 1) as f64 / (self.rows + 1) as f64),
        })
    }
}

/// Where a row sorts against another under the keys, with the nulls placed before the direction
/// is applied, as [`crate::general`] sorts the rows of an ordered call.
fn placed(keys: &[StateKey], row: &[Value], other: &[Value]) -> Result<Ordering> {
    for (key, (left, right)) in keys.iter().zip(row.iter().zip(other)) {
        let mut ordering = order_with_nulls(left, right, key.nulls_first)?;
        if key.descending && !left.is_null() && !right.is_null() {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

/// The values of a group in the order of the call's key.
fn sorted(held: &Held, descending: bool) -> Result<Vec<Value>> {
    let mut values = match held {
        Held::Empty => Vec::new(),
        Held::Wholes { values, whole } => {
            let mut numbers = values.clone();
            numbers.sort_unstable();
            numbers.into_iter().map(|n| whole.value(n)).collect()
        }
        Held::Reals(values) => {
            let mut numbers = values.clone();
            numbers.sort_unstable_by(|left, right| float_order(*left, *right));
            numbers.into_iter().map(Value::Double).collect()
        }
        Held::Values(values) => {
            let mut values = values.clone();
            let mut failure = None;
            values.sort_by(|left, right| {
                order(left, right).unwrap_or_else(|error| {
                    failure.get_or_insert(error);
                    Ordering::Equal
                })
            });
            if let Some(error) = failure {
                return Err(error);
            }
            values
        }
    };
    if descending {
        values.reverse();
    }
    Ok(values)
}

/// The fraction as a double, refused outside 0 and 1 in PostgreSQL's words.
fn share(fraction: &Value) -> Result<f64> {
    let share = approximate(fraction)
        .ok_or_else(|| Error::internal(format!("a percentile of {fraction}")))?;
    if (0.0..=1.0).contains(&share) {
        return Ok(share);
    }
    // PostgreSQL's own `%g` spells the values that are not numbers as words.
    let written = match printf("%g", &[Value::Double(share)])? {
        _ if share.is_nan() => "NaN".to_string(),
        _ if share.is_infinite() => if share > 0.0 { "Infinity" } else { "-Infinity" }.to_string(),
        Value::Varchar(text) => text.to_string(),
        other => other.to_string(),
    };
    Err(Error::out_of_range(format!("percentile value {written} is not between 0 and 1"))
        .state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE))
}

/// Refuses an array that holds a fraction outside 0 and 1, which PostgreSQL does for every one of
/// them before it reads a row.
fn checked(fraction: &Value) -> Result<()> {
    match fraction {
        Value::List { values, .. } => values.iter().try_for_each(checked),
        Value::Null => Ok(()),
        one => share(one).map(|_| ()),
    }
}

/// The answer for an array of fractions, in its shape, with a null for a null fraction.
fn picked(
    fraction: &Value,
    values: &[Value],
    continuous: bool,
    returns: &LogicalType,
) -> Result<Value> {
    match (fraction, returns) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::List { values: fractions, .. }, LogicalType::List(element)) => {
            let mut answers = Vec::with_capacity(fractions.len());
            for one in fractions {
                answers.push(picked(one, values, continuous, element)?);
            }
            Ok(Value::List { element: element.as_ref().clone(), values: answers })
        }
        (one, _) => percentile(values, share(one)?, continuous),
    }
}

/// The percentile `share` of values in order, which are not empty.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "PostgreSQL finds the rows with doubles too, and a share in [0, 1] keeps them in range"
)]
fn percentile(values: &[Value], share: f64, continuous: bool) -> Result<Value> {
    let n = values.len();
    if !continuous {
        let row = (share * n as f64).ceil() as usize;
        return Ok(values[row.clamp(1, n) - 1].clone());
    }
    let along = share * (n - 1) as f64;
    let (first, second) = (along.floor() as usize, along.ceil() as usize);
    if first == second {
        return Ok(values[first].clone());
    }
    lerp(&values[first], &values[second], along - along.floor())
}

/// The value `along` of the way from `lo` to `hi`, as `float8_lerp` and `interval_lerp` find it.
fn lerp(lo: &Value, hi: &Value, along: f64) -> Result<Value> {
    match (lo, hi) {
        (Value::Double(lo), Value::Double(hi)) => Ok(Value::Double(lo + along * (hi - lo))),
        (Value::Interval { .. }, Value::Interval { .. }) => {
            let difference = combine(hi, lo, true)?;
            combine(&scaled(&difference, &Value::Double(along), false)?, lo, false)
        }
        _ => Err(Error::internal(format!("percentile_cont between {lo} and {hi}"))),
    }
}

/// The value seen most often, and of those the one that sorts first, as `mode_final` finds it.
fn mode(values: &[Value]) -> Result<Value> {
    let mut best: Option<(&Value, usize)> = None;
    let mut at = 0;
    while at < values.len() {
        let mut end = at + 1;
        while end < values.len() && order(&values[at], &values[end])? == Ordering::Equal {
            end += 1;
        }
        if best.is_none_or(|(_, count)| end - at > count) {
            best = Some((&values[at], end - at));
        }
        at = end;
    }
    Ok(best.map_or(Value::Null, |(value, _)| value.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(descending: bool) -> StateKey {
        StateKey { descending, nulls_first: false, column: 0 }
    }

    fn run(name: &str, keys: &[StateKey], returns: &LogicalType, rows: &[Vec<Value>]) -> Value {
        let mut state = OrderedSet::named(name, keys, returns).expect("a known name");
        for row in rows {
            state.update(row).expect("a row folds in");
        }
        state.finish().expect("an answer")
    }

    fn doubles(values: &[f64], fraction: &Value) -> Vec<Vec<Value>> {
        values.iter().map(|&value| vec![Value::Double(value), fraction.clone()]).collect()
    }

    #[test]
    fn a_continuous_percentile_goes_between_two_rows_as_postgres_does() {
        let rows = doubles(&[5.0, 1.0, 3.0, 2.0, 4.0], &Value::Double(0.1));
        let answer = run("percentile_cont_final", &[key(false)], &LogicalType::Double, &rows);
        assert_eq!(answer, Value::Double(1.4));
        let rows = doubles(&[1.0, 3.0, 5.0, 7.0], &Value::Double(0.25));
        let answer = run("percentile_cont_final", &[key(false)], &LogicalType::Double, &rows);
        assert_eq!(answer, Value::Double(2.5));
        let answer = run("percentile_cont_final", &[key(true)], &LogicalType::Double, &rows);
        assert_eq!(answer, Value::Double(5.5));
    }

    #[test]
    fn a_discrete_percentile_is_the_row_at_the_ceiling() {
        let rows: Vec<Vec<Value>> =
            (1..=10).map(|n| vec![Value::Integer(n), Value::Double(0.25)]).collect();
        let answer = run("percentile_disc_final", &[key(false)], &LogicalType::Integer, &rows);
        assert_eq!(answer, Value::Integer(3));
        let rows: Vec<Vec<Value>> =
            (1..=10).map(|n| vec![Value::Integer(n), Value::Double(0.0)]).collect();
        let answer = run("percentile_disc_final", &[key(false)], &LogicalType::Integer, &rows);
        assert_eq!(answer, Value::Integer(1));
    }

    #[test]
    fn an_array_of_fractions_answers_an_array_with_its_nulls() {
        let fractions = Value::List {
            element: LogicalType::Double,
            values: vec![Value::Null, Value::Double(1.0), Value::Double(0.5)],
        };
        let rows: Vec<Vec<Value>> =
            (0..4).map(|n| vec![Value::Integer(n), fractions.clone()]).collect();
        let returns = LogicalType::List(Box::new(LogicalType::Integer));
        let answer = run("percentile_disc_final", &[key(false)], &returns, &rows);
        let expected = Value::List {
            element: LogicalType::Integer,
            values: vec![Value::Null, Value::Integer(3), Value::Integer(1)],
        };
        assert_eq!(answer, expected);
    }

    #[test]
    fn a_fraction_outside_zero_and_one_is_refused_in_the_words_of_postgres() {
        let rows = doubles(&[1.0], &Value::Double(1.5));
        let mut state =
            OrderedSet::named("percentile_cont_final", &[key(false)], &LogicalType::Double)
                .expect("a known name");
        state.update(&rows[0]).expect("a row folds in");
        let error = state.finish().expect_err("out of range");
        assert!(error.to_string().contains("percentile value 1.5 is not between 0 and 1"));
        let rows = doubles(&[1.0], &Value::Null);
        let answer = run("percentile_cont_final", &[key(false)], &LogicalType::Double, &rows);
        assert_eq!(answer, Value::Null);
    }

    #[test]
    fn the_mode_breaks_a_tie_by_the_order_of_the_key() {
        let rows: Vec<Vec<Value>> =
            [3, 1, 3, 1, 2].iter().map(|&n| vec![Value::Integer(n)]).collect();
        let answer = run("mode_final", &[key(false)], &LogicalType::Integer, &rows);
        assert_eq!(answer, Value::Integer(1));
        let answer = run("mode_final", &[key(true)], &LogicalType::Integer, &rows);
        assert_eq!(answer, Value::Integer(3));
    }

    #[test]
    fn the_hypothetical_row_is_placed_among_the_rows() {
        let rows: Vec<Vec<Value>> = [1, 1, 2, 2, 3, 3, 4]
            .iter()
            .map(|&n| vec![Value::Integer(n), Value::Integer(3)])
            .collect();
        let keys = [key(false)];
        let rank = run("hypothetical_rank_final", &keys, &LogicalType::BigInt, &rows);
        assert_eq!(rank, Value::BigInt(5));
        let dense = run("hypothetical_dense_rank_final", &keys, &LogicalType::BigInt, &rows);
        assert_eq!(dense, Value::BigInt(3));
        let cume = run("hypothetical_cume_dist_final", &keys, &LogicalType::Double, &rows);
        assert_eq!(cume, Value::Double(0.875));
        let percent = run("hypothetical_percent_rank_final", &keys, &LogicalType::Double, &[]);
        assert_eq!(percent, Value::Double(0.0));
    }
}
