//! `entropy`, which only needs to know how often each value came up.
//!
//! The pin's optimizer rewrites `entropy(x)` into a count per value of `x` and then
//! `log2(total) - sum(f * log2(f)) / total` over those counts, so what a group holds here is the
//! counts and not the values. A column of numbers is counted in a hash map keyed by the number,
//! text by the text, and anything else in a list of distinct values kept sorted, with the values
//! that arrived since the last sort waiting beside it until there are enough of them to be worth
//! sorting in.

use std::cmp::Ordering;
use std::collections::HashMap;

use rudb_common::{Result, Value};

use crate::compare::order;
use crate::quantile::{Column, Whole};

/// How many unsorted values [`Tally::Values`] lets build up before it sorts them into its counts.
const PENDING: usize = 4096;

/// The count of each distinct value a group has seen.
#[derive(Debug, Clone, Default)]
pub(crate) enum Tally {
    #[default]
    Empty,
    /// Whole numbers of one type, the way [`Whole::of`] reads them.
    Wholes {
        counts: HashMap<i64, u64>,
        whole: Whole,
    },
    /// Doubles, by their bits once every zero is `0.0` and every NaN is the one NaN, because the
    /// pin groups `-0.0` with `0.0` and one NaN with another.
    Reals(HashMap<u64, u64>),
    Words(HashMap<Box<str>, u64>),
    /// Anything else, as distinct values in order with their counts and the values still to be
    /// sorted in.
    Values {
        counted: Vec<(Value, u64)>,
        pending: Vec<Value>,
    },
}

impl Tally {
    /// Counts a value that is not null.
    pub(crate) fn push(&mut self, value: &Value) -> Result<()> {
        if let Self::Empty = self {
            *self = match (Whole::of(value), value) {
                (Some((whole, _)), _) => Self::Wholes { counts: HashMap::new(), whole },
                (None, Value::Double(_)) => Self::Reals(HashMap::new()),
                (None, Value::Varchar(_)) => Self::Words(HashMap::new()),
                _ => Self::Values { counted: Vec::new(), pending: Vec::new() },
            };
        }
        match (&mut *self, value) {
            (Self::Wholes { counts, whole }, value) => match Whole::of(value) {
                Some((kind, n)) if kind == *whole => *counts.entry(n).or_default() += 1,
                _ => self.spilled()?.push(value.clone()),
            },
            (Self::Reals(counts), Value::Double(real)) => {
                *counts.entry(bits(*real)).or_default() += 1
            }
            (Self::Words(counts), Value::Varchar(text)) => {
                if let Some(count) = counts.get_mut(text.as_str()) {
                    *count += 1;
                } else {
                    counts.insert(text.as_str().into(), 1);
                }
            }
            (Self::Values { .. }, value) => self.spilled()?.push(value.clone()),
            (_, value) => self.spilled()?.push(value.clone()),
        }
        self.settle(false)
    }

    /// Counts the row of a column a [`Column`] reads.
    pub(crate) fn push_column(&mut self, column: Column<'_>, row: usize) -> Result<()> {
        match (&mut *self, column) {
            (Self::Wholes { counts, whole }, Column::Wholes(kind, numbers)) if *whole == kind => {
                *counts.entry(numbers.at(row)).or_default() += 1;
                Ok(())
            }
            (Self::Reals(counts), Column::Reals(reals)) => {
                *counts.entry(bits(reals[row])).or_default() += 1;
                Ok(())
            }
            (_, Column::Wholes(kind, numbers)) => self.push(&kind.value(numbers.at(row))),
            (_, Column::Reals(reals)) => self.push(&Value::Double(reals[row])),
            (_, Column::Flags(flags)) => self.push(&Value::Boolean(flags[row])),
        }
    }

    /// Adds the counts of another group of the same call to these.
    pub(crate) fn append(&mut self, other: &Self) -> Result<()> {
        match (&mut *self, other) {
            (_, Self::Empty) => Ok(()),
            (Self::Empty, other) => {
                other.clone_into(self);
                Ok(())
            }
            (Self::Wholes { counts, whole }, Self::Wholes { counts: more, whole: theirs })
                if whole == theirs =>
            {
                for (&n, &count) in more {
                    *counts.entry(n).or_default() += count;
                }
                Ok(())
            }
            (Self::Reals(counts), Self::Reals(more)) => {
                for (&real, &count) in more {
                    *counts.entry(real).or_default() += count;
                }
                Ok(())
            }
            (Self::Words(counts), Self::Words(more)) => {
                for (text, &count) in more {
                    *counts.entry(text.clone()).or_default() += count;
                }
                Ok(())
            }
            (_, other) => {
                let mut theirs = other.distinct()?;
                sort(&mut theirs)?;
                self.spilled()?;
                self.settle(true)?;
                if let Self::Values { counted, .. } = self {
                    *counted = merge(std::mem::take(counted), theirs)?;
                }
                Ok(())
            }
        }
    }

    /// The entropy of the group in bits, zero when it has no values.
    pub(crate) fn entropy(&self) -> Result<Value> {
        let counts: Vec<u64> = match self {
            Self::Empty => Vec::new(),
            Self::Wholes { counts, .. } => counts.values().copied().collect(),
            Self::Reals(counts) => counts.values().copied().collect(),
            Self::Words(counts) => counts.values().copied().collect(),
            Self::Values { .. } => self.distinct()?.into_iter().map(|(_, count)| count).collect(),
        };
        if counts.is_empty() {
            return Ok(Value::Double(0.0));
        }
        #[expect(clippy::cast_precision_loss, reason = "the pin sums the counts as doubles too")]
        let (total, weighted) = counts.iter().fold((0.0, 0.0), |(total, weighted), &count| {
            let count = count as f64;
            (total + count, weighted + count * count.log2())
        });
        Ok(Value::Double(total.log2() - weighted / total))
    }

    /// Every distinct value with its count, in the order the values sort in.
    pub(crate) fn sorted(&self) -> Result<Vec<(Value, u64)>> {
        let mut all = self.distinct()?;
        if !matches!(self, Self::Values { .. }) {
            sort(&mut all)?;
        }
        Ok(all)
    }

    /// Every distinct value with its count, in no particular order.
    fn distinct(&self) -> Result<Vec<(Value, u64)>> {
        Ok(match self {
            Self::Empty => Vec::new(),
            Self::Wholes { counts, whole } => {
                counts.iter().map(|(&n, &count)| (whole.value(n), count)).collect()
            }
            Self::Reals(counts) => counts
                .iter()
                .map(|(&real, &count)| (Value::Double(f64::from_bits(real)), count))
                .collect(),
            Self::Words(counts) => counts
                .iter()
                .map(|(text, &count)| (Value::Varchar(text.to_string()), count))
                .collect(),
            Self::Values { counted, pending } => {
                let mut all = Self::Values { counted: counted.clone(), pending: pending.clone() };
                all.settle(true)?;
                match all {
                    Self::Values { counted, .. } => counted,
                    _ => Vec::new(),
                }
            }
        })
    }

    /// Turns this into [`Tally::Values`] and hands back the values waiting to be sorted in.
    fn spilled(&mut self) -> Result<&mut Vec<Value>> {
        if !matches!(self, Self::Values { .. }) {
            let mut counted = self.distinct()?;
            sort(&mut counted)?;
            *self = Self::Values { counted, pending: Vec::new() };
        }
        match self {
            Self::Values { pending, .. } => Ok(pending),
            _ => unreachable!("a tally was just made one of values"),
        }
    }

    /// Sorts the waiting values into the counts, once there are enough of them or when `now`.
    fn settle(&mut self, now: bool) -> Result<()> {
        let Self::Values { counted, pending } = self else { return Ok(()) };
        if pending.is_empty() || (!now && pending.len() < PENDING) {
            return Ok(());
        }
        let mut arrived: Vec<(Value, u64)> = pending.drain(..).map(|value| (value, 1)).collect();
        sort(&mut arrived)?;
        *counted = merge(std::mem::take(counted), arrived)?;
        Ok(())
    }
}

/// Two lists of distinct values in order with their counts, as one.
fn merge(mine: Vec<(Value, u64)>, theirs: Vec<(Value, u64)>) -> Result<Vec<(Value, u64)>> {
    let mut merged: Vec<(Value, u64)> = Vec::with_capacity(mine.len() + theirs.len());
    let (mut mine, mut theirs) = (mine.into_iter().peekable(), theirs.into_iter().peekable());
    loop {
        let next = match (mine.peek(), theirs.peek()) {
            (None, None) => break,
            (Some(_), None) => mine.next(),
            (None, Some(_)) => theirs.next(),
            (Some((left, _)), Some((right, _))) => {
                if order(left, right)? == Ordering::Greater {
                    theirs.next()
                } else {
                    mine.next()
                }
            }
        };
        let Some((value, count)) = next else { break };
        match merged.last_mut() {
            Some((last, total)) if order(last, &value)? == Ordering::Equal => *total += count,
            _ => merged.push((value, count)),
        }
    }
    Ok(merged)
}

/// The key a double is counted under.
fn bits(real: f64) -> u64 {
    if real.is_nan() {
        f64::NAN.to_bits()
    } else if real == 0.0 {
        0.0_f64.to_bits()
    } else {
        real.to_bits()
    }
}

/// Sorts values with their counts into order.
fn sort(values: &mut [(Value, u64)]) -> Result<()> {
    let mut failure = None;
    values.sort_by(|(left, _), (right, _)| {
        order(left, right).unwrap_or_else(|error| {
            failure.get_or_insert(error);
            Ordering::Equal
        })
    });
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::*;

    fn entropy_of(values: &[Value]) -> f64 {
        let mut tally = Tally::Empty;
        for value in values {
            tally.push(value).expect("counts");
        }
        let Value::Double(bits) = tally.entropy().expect("answers") else { panic!("not a double") };
        bits
    }

    #[test]
    fn a_tally_counts_numbers_zeros_and_nans_the_way_the_pin_groups_them() {
        let wholes = [1, 2, 2, 3].map(Value::Integer);
        assert!((entropy_of(&wholes) - 1.5).abs() < 1e-12);
        let reals = [0.0, -0.0, f64::NAN, -f64::NAN].map(Value::Double);
        assert!((entropy_of(&reals) - 1.0).abs() < 1e-12);
        assert!(entropy_of(&[]).abs() < f64::EPSILON);
    }

    fn list(n: i64) -> Value {
        Value::List { element: LogicalType::BigInt, values: vec![Value::BigInt(n)] }
    }

    #[test]
    fn a_tally_of_other_values_sorts_them_in_and_merges_with_another() {
        let lists: Vec<Value> =
            (0..3 * PENDING).map(|at| list(i64::try_from(at % 4).expect("small"))).collect();
        assert!((entropy_of(&lists) - 2.0).abs() < 1e-12);
        let (mut mine, mut theirs) = (Tally::Empty, Tally::Empty);
        for n in [0, 1] {
            mine.push(&list(n)).expect("counts");
        }
        for n in [1, 1] {
            theirs.push(&list(n)).expect("counts");
        }
        mine.append(&theirs).expect("merges");
        let Value::Double(bits) = mine.entropy().expect("answers") else { panic!("not a double") };
        assert!((bits - 0.811_278_124_459_132_8).abs() < 1e-12, "{bits}");
    }
}
