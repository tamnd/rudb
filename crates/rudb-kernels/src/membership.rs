//! `IN` over a list the query wrote out.
//!
//! The binder has no `IN` node. `x IN (1, 2, 3)` is bound as `x = 1 OR x = 2 OR x = 3` and
//! `x NOT IN (1, 2, 3)` as `x <> 1 AND x <> 2 AND x <> 3`, which is the right thing for the binder
//! to do because it means nothing after it has to know a second set of rules for null. What it
//! costs is a pass over the column and an output vector per list entry, and TPC-H query 16 has
//! eight entries in one list.
//!
//! This file is the other end of that. A caller that can see the whole conjunction folds it back
//! into a set, and then the column is read once and each row is one lookup. What is being removed
//! is the pass and the allocation per entry rather than the comparison per row, which is why two
//! entries is already worth folding rather than four or eight.
//!
//! # What it will not fold
//!
//! Whole numbers and strings, and nothing else. A float will not fold because DuckDB's `=` on a
//! float is not the equality a hash set has: it says that two nans are equal and that a positive
//! and a negative zero are equal, and the second one also breaks hashing rather than only the
//! comparison. A decimal will not fold because two unscaled integers at two scales are the same
//! number, and the check that the scales match is not worth writing for a list nobody writes. An
//! interval will not fold because interval equality is by length and a month is not thirty days.
//! Anything this refuses stays the `OR` the binder built, which is correct and is counted.

use std::collections::HashSet;

use rudb_common::{LogicalType, Result, Value};
use rudb_vector::{Data, Form, Validity, Vector};

use crate::shape::{first, identity, nulls_of, single};

/// The list of an `IN`, in the shape a loop can look a row up in.
///
/// Built once when the pipeline is built, because the list is literals the user wrote and cannot
/// change from chunk to chunk. This is the same idea as [`crate::prepare`] and is a separate type
/// only because an `IN` is not a function call by the time it reaches here.
#[derive(Debug)]
pub struct Members {
    held: Held,
    /// Whether the list held a null.
    ///
    /// A row that is not in the list is null rather than false when it did, because the row might
    /// have equalled whatever the null stands for. This is the whole of the difference between an
    /// `IN` and a set lookup and it is the thing a hand written version gets wrong.
    has_null: bool,
    /// Whether this was a `NOT IN`, which the binder wrote as an `AND` of inequalities.
    negated: bool,
}

/// The set itself, in the one layout per kind of value that hashes the way SQL compares.
#[derive(Debug)]
enum Held {
    /// Every integral type and the three whole calendar ones, widened to the widest signed integer.
    /// Widening is exact for all of them, and the binder has already cast the column and the list
    /// to one type, so two entries that differ here differ in SQL too.
    Whole(HashSet<i128>),
    /// Strings, compared by bytes, which is what DuckDB's `=` on a varchar does.
    Text(HashSet<String>),
}

impl Members {
    /// The list as a set, or `None` for a list this file will not fold.
    ///
    /// `None` covers a list of fewer than two entries, which is not worth a set, a list holding a
    /// kind of value that does not hash the way SQL compares, and a list mixing two kinds, which
    /// the binder does not produce but which is cheaper to refuse than to reason about.
    #[must_use]
    pub fn of(values: &[Value], negated: bool) -> Option<Self> {
        if values.len() < 2 {
            return None;
        }
        let mut whole: HashSet<i128> = HashSet::new();
        let mut text: HashSet<String> = HashSet::new();
        let mut has_null = false;
        let mut kind: Option<std::mem::Discriminant<Value>> = None;
        for value in values {
            if matches!(value, Value::Null) {
                has_null = true;
                continue;
            }
            // One kind for the whole list. The binder casts every entry to the type the comparison
            // happens at, so a list that reaches here is already uniform, and a list that is not is
            // one this file has no business guessing about.
            let held = std::mem::discriminant(value);
            if *kind.get_or_insert(held) != held {
                return None;
            }
            match value {
                Value::Varchar(held) => {
                    text.insert(held.clone());
                }
                other => {
                    whole.insert(number(other)?);
                }
            }
        }
        let held = if text.is_empty() {
            if whole.is_empty() {
                // Every entry was null, so every row is null and there is nothing to look up. Rare
                // enough that the `OR` can have it.
                return None;
            }
            Held::Whole(whole)
        } else {
            Held::Text(text)
        };
        Some(Self { held, has_null, negated })
    }

    /// How many distinct values the list holds, for a caller that wants to say so.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.held {
            Held::Whole(set) => set.len(),
            Held::Text(set) => set.len(),
        }
    }

    /// Whether the list holds no value at all, which [`Members::of`] never builds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A value as the integer the set is keyed on, or `None` for a kind that does not belong in one.
fn number(value: &Value) -> Option<i128> {
    match *value {
        Value::TinyInt(held) => Some(i128::from(held)),
        Value::SmallInt(held) => Some(i128::from(held)),
        Value::Integer(held) | Value::Date(held) => Some(i128::from(held)),
        Value::BigInt(held) | Value::Time(held) | Value::Timestamp(held) => Some(i128::from(held)),
        Value::HugeInt(held) => Some(held),
        Value::UTinyInt(held) => Some(i128::from(held)),
        Value::USmallInt(held) => Some(i128::from(held)),
        Value::UInteger(held) => Some(i128::from(held)),
        Value::UBigInt(held) => Some(i128::from(held)),
        _ => None,
    }
}

/// Which rows of `input` are in the list.
///
/// # Errors
///
/// If the answer vector cannot be built, which is the same check every kernel here makes.
pub fn in_set(input: &Vector, members: &Members, returns: &LogicalType) -> Result<Vector> {
    let rows = input.len();
    let base = nulls_of(input);
    match input.form() {
        Form::Flat => match input.data() {
            Some(data) => look(data, identity, members, &base, rows, returns),
            None => row_at_a_time(input, members, &base, rows, returns),
        },
        Form::Dictionary | Form::Rle => {
            let Some((codes, values)) = input.positions() else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            let Some(data) = values.data().filter(|_| codes.len() >= rows) else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            let at = move |index: usize| codes[index] as usize;
            look(data, at, members, &base, rows, returns)
        }
        Form::Constant => {
            let Some(value) = input.constant_value() else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            let Some(held) = single(input.logical_type(), value) else {
                return row_at_a_time(input, members, &base, rows, returns);
            };
            match held.data() {
                Some(data) => look(data, first, members, &base, rows, returns),
                None => row_at_a_time(input, members, &base, rows, returns),
            }
        }
        _ => row_at_a_time(input, members, &base, rows, returns),
    }
}

/// The lookup loop, once per physical layout the column can arrive in.
///
/// The index mapping is a generic parameter rather than a function pointer for the reason the
/// `by_form` macro in `scalar` gives, which is that a function pointer here is an indirect call per
/// row.
fn look<A: Fn(usize) -> usize>(
    data: &Data,
    at: A,
    members: &Members,
    base: &Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Vector> {
    match (&members.held, data) {
        (Held::Text(set), Data::Varlen(column)) => answer(rows, base, members, returns, |index| {
            column.get(at(index)).is_some_and(|text| set.contains(text))
        }),
        (Held::Whole(set), Data::Int8(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int16(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int32(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int64(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::Int128(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt8(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt16(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt32(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        (Held::Whole(set), Data::UInt64(held)) => {
            answer(rows, base, members, returns, |index| holds(set, held.as_slice(), at(index)))
        }
        // A layout the set cannot be keyed on, which means the list and the column disagree about
        // what they hold. `Members::of` refuses the lists that would get here, so this is the arm
        // that keeps that true rather than assumed.
        _ => Err(rudb_common::Error::internal(format!(
            "an IN list over a column this kernel does not read, which is {returns}"
        ))),
    }
}

/// Whether the set holds the value at `index`, for any integer narrower than the key.
fn holds<T: Copy>(set: &HashSet<i128>, values: &[T], index: usize) -> bool
where
    i128: From<T>,
{
    values.get(index).is_some_and(|&held| set.contains(&i128::from(held)))
}

/// The answer, given a lookup that says whether a row is in the list.
fn answer(
    rows: usize,
    base: &Validity,
    members: &Members,
    returns: &LogicalType,
    found: impl Fn(usize) -> bool,
) -> Result<Vector> {
    let mut out = vec![false; rows];
    let mut live = vec![false; rows];
    for index in 0..rows {
        if !base.is_valid(index) {
            continue;
        }
        let hit = found(index);
        // A miss against a list with a null in it is null and not false, because the row might have
        // equalled whatever that null stands for. A hit is a hit whatever else the list holds.
        live[index] = hit || !members.has_null;
        out[index] = hit != members.negated;
    }
    let validity = Validity::from_run(&live).normalize(rows);
    Ok(Vector::flat(returns.clone(), Data::Bool(out.into()))?.with_validity(validity))
}

/// The path for a form or a layout with no loop above, which reads a value per row.
///
/// It counts itself nowhere, because there is nothing here for the fallback table to tell anybody:
/// `Members::of` decides what folds, so a column that reaches this is one the fold should not have
/// happened for, and the answer to that is a line in `Members::of` rather than a number in a report.
fn row_at_a_time(
    input: &Vector,
    members: &Members,
    base: &Validity,
    rows: usize,
    returns: &LogicalType,
) -> Result<Vector> {
    let held: Vec<Value> = (0..rows).map(|index| input.value_at(index)).collect();
    answer(rows, base, members, returns, |index| match (&members.held, &held[index]) {
        (Held::Text(set), Value::Varchar(text)) => set.contains(text.as_str()),
        (Held::Whole(set), value) => number(value).is_some_and(|held| set.contains(&held)),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::Vector;

    use super::{Members, in_set};

    /// What the kernel answers for each row, as the values a caller would read back.
    fn over(input: &Vector, list: &[Value], negated: bool) -> Vec<Value> {
        let members = Members::of(list, negated).expect("this list folds");
        let answer = in_set(input, &members, &LogicalType::Boolean).expect("the lookup runs");
        (0..input.len()).map(|row| answer.value_at(row)).collect()
    }

    fn numbers() -> Vector {
        Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(7), Value::Null, Value::Integer(3)],
        )
        .expect("four integers")
    }

    #[test]
    fn a_row_in_the_list_is_true_and_a_row_outside_it_is_false() {
        assert_eq!(
            over(&numbers(), &[Value::Integer(1), Value::Integer(3)], false),
            [Value::Boolean(true), Value::Boolean(false), Value::Null, Value::Boolean(true)]
        );
    }

    #[test]
    fn a_not_in_is_the_same_lookup_read_the_other_way() {
        assert_eq!(
            over(&numbers(), &[Value::Integer(1), Value::Integer(3)], true),
            [Value::Boolean(false), Value::Boolean(true), Value::Null, Value::Boolean(false)]
        );
    }

    /// The rule that separates a set lookup from an `IN`. `7 IN (1, NULL)` is null rather than
    /// false, because the row might have equalled whatever the null stands for, and `7 NOT IN
    /// (1, NULL)` is null for the same reason.
    #[test]
    fn a_miss_against_a_list_with_a_null_in_it_is_null() {
        let list = [Value::Integer(1), Value::Null, Value::Integer(3)];
        assert_eq!(
            over(&numbers(), &list, false),
            [Value::Boolean(true), Value::Null, Value::Null, Value::Boolean(true)]
        );
        assert_eq!(
            over(&numbers(), &list, true),
            [Value::Boolean(false), Value::Null, Value::Null, Value::Boolean(false)]
        );
    }

    #[test]
    fn a_dictionary_column_is_read_through_its_codes() {
        let values = Vector::from_values(
            LogicalType::Varchar,
            &[Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Null],
        )
        .expect("builds");
        let text = Vector::dictionary(vec![0, 2, 1, 0], values).expect("codes are in range");
        let list = [Value::Varchar("a".into()), Value::Varchar("c".into())];
        assert_eq!(
            over(&text, &list, false),
            [Value::Boolean(true), Value::Null, Value::Boolean(false), Value::Boolean(true)]
        );
    }

    #[test]
    fn a_constant_column_answers_every_row_the_same() {
        let held = Vector::constant(LogicalType::Integer, Value::Integer(3), 3);
        let list = [Value::Integer(1), Value::Integer(3)];
        assert_eq!(over(&held, &list, false), vec![Value::Boolean(true); 3]);
    }

    #[test]
    fn a_run_length_column_reads_the_same_as_the_flat_one_it_stands_for() {
        let flat = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(1), Value::Integer(7), Value::Integer(7)],
        )
        .expect("four integers");
        let runs = flat.clone().run_encoded().expect("two runs");
        let list = [Value::Integer(1), Value::Integer(3)];
        assert_eq!(over(&runs, &list, false), over(&flat, &list, false));
    }

    #[test]
    fn a_list_of_one_is_left_alone_because_a_comparison_is_already_that() {
        assert!(Members::of(&[Value::Integer(1)], false).is_none());
    }

    #[test]
    fn a_list_of_floats_does_not_fold() {
        // Two nans are equal to DuckDB's `=` and not to a hash set, and a positive and a negative
        // zero are equal to both but hash differently. Neither is worth a special case.
        assert!(Members::of(&[Value::Double(1.0), Value::Double(2.0)], false).is_none());
    }

    #[test]
    fn a_list_of_two_kinds_does_not_fold() {
        let mixed = [Value::Integer(1), Value::Varchar("a".into())];
        assert!(Members::of(&mixed, false).is_none());
    }

    #[test]
    fn a_list_of_nothing_but_nulls_does_not_fold() {
        assert!(Members::of(&[Value::Null, Value::Null], false).is_none());
    }

    #[test]
    fn a_list_says_how_many_distinct_values_it_holds() {
        let list = [Value::Integer(1), Value::Integer(1), Value::Integer(2), Value::Null];
        let members = Members::of(&list, false).expect("this list folds");
        assert_eq!(members.len(), 2);
        assert!(!members.is_empty());
    }
}
