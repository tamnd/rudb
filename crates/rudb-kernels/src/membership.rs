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
use std::sync::{Arc, OnceLock};

use rudb_common::{LogicalType, Result, Value};
use rudb_vector::{Data, Form, Validity, Vector};

use crate::peel::{Found, Peel, search};
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
    /// Where the list's values sit in the dictionary a column arrives with, when that dictionary
    /// came with its own sorted order. Searched once for the whole query.
    sought: OnceLock<Sought>,
    /// The per value memo for a dictionary that has no order to search.
    peel: Peel,
}

/// The codes one list sits at in one dictionary, searched once and remembered.
///
/// The same idea as [`crate::peel::Lookup`], which does it for a single literal, and a separate
/// type because a list is several literals and the answer is therefore a set of codes rather than
/// one. Short, since it is as long as the list the query wrote out.
#[derive(Debug)]
struct Sought {
    /// The dictionary these codes are in, recognised by pointer the way a peel does it.
    dictionary: Arc<Vector>,
    /// The codes the list's values sit at, and no entry at all for a value the dictionary does not
    /// hold, since no row can be that value.
    codes: Vec<u32>,
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
        Some(Self { held, has_null, negated, sought: OnceLock::new(), peel: Peel::default() })
    }

    /// The codes this list's values sit at in `column`'s dictionary, or `None` when there is no
    /// sorted order to find them with.
    ///
    /// This is the whole of what a sorted dictionary buys an `IN`. The list is a handful of
    /// literals and the dictionary knows where each of them sits, so one binary search per literal
    /// for the whole query turns the predicate into a code against a handful of codes, and no
    /// value is read at any point. `l_shipmode IN ('MAIL', 'SHIP')` over SF1 lineitem is two
    /// searches of a seven entry dictionary rather than six million string comparisons.
    ///
    /// Text only, because the search is over bytes. A list of numbers against a dictionary is left
    /// to the loop below, which reads the codes' values as a run and is already one lookup a row.
    fn sought(&self, column: &Vector) -> Option<Result<&[u32]>> {
        let Held::Text(set) = &self.held else { return None };
        let (_, dictionary) = column.shared_dictionary_parts()?;
        if self.sought.get().is_none() {
            let ranks = dictionary.ranks()?;
            let mut codes = Vec::with_capacity(set.len());
            for text in set {
                match search(dictionary, ranks, text.as_bytes()) {
                    Ok(Found::At(code)) => codes.push(code),
                    Ok(Found::Absent) => {}
                    // Returned rather than remembered, so a caller that retries gets the error
                    // again rather than a wrong answer cached from a half finished search.
                    Err(error) => return Some(Err(error)),
                }
            }
            // Two threads that get here at once do the same searches and set the same codes, and
            // the one that loses the race drops its own copy of them.
            let _ = self.sought.set(Sought { dictionary: Arc::clone(dictionary), codes });
        }
        // Read back what is actually there rather than what this call built, and check it belongs
        // to the dictionary in hand, which is what declines a second column at the same node.
        let memo = self.sought.get()?;
        Arc::ptr_eq(&memo.dictionary, dictionary).then_some(Ok(memo.codes.as_slice()))
    }

    /// Whether the value at `code` of `dictionary` is in the list, for the memo to remember.
    fn at_code(&self, dictionary: &Vector, code: usize) -> Result<bool> {
        Ok(match &self.held {
            Held::Text(set) => dictionary
                .try_bytes_at(code)?
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .is_some_and(|text| set.contains(text)),
            Held::Whole(set) => {
                number(&dictionary.try_value_at(code)?).is_some_and(|held| set.contains(&held))
            }
        })
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
        Value::BigInt(held)
        | Value::Time(held)
        | Value::TimeTz(held)
        | Value::Timestamp(held)
        | Value::TimestampTz(held) => Some(i128::from(held)),
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
            // A dictionary column asks the same question about the same value once a row, so both
            // paths here ask it once a value instead. The search goes first because it reads no
            // value at all, and the memo takes the dictionaries that have no order to search.
            if let Some(found) = members.sought(input) {
                let found = found?;
                let (codes, _) = input.shared_dictionary_parts().ok_or_else(|| {
                    rudb_common::Error::internal("a searched column lost its codes")
                })?;
                if codes.len() >= rows {
                    return answer(rows, &base, members, returns, |index| {
                        found.contains(&codes[index])
                    });
                }
            }
            if let Some(found) = members
                .peel
                .answer(input, rows, identity, |dictionary, code| members.at_code(dictionary, code))
            {
                let found = found?;
                return answer(rows, &base, members, returns, |index| found[index]);
            }
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
    let held: Vec<Value> =
        (0..rows).map(|index| input.try_value_at(index)).collect::<Result<_>>()?;
    answer(rows, base, members, returns, |index| match (&members.held, &held[index]) {
        (Held::Text(set), Value::Varchar(text)) => set.contains(text.as_str()),
        (Held::Whole(set), value) => number(value).is_some_and(|held| set.contains(&held)),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as Memory};

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

    /// Values a storage reader hands over one at a time, which know the order the writer sorted
    /// them into. This is the shape a text column of a native file arrives in, and the whole point
    /// of the shape is that nothing has to read a value to find out where one sits.
    #[derive(Debug)]
    struct Filed {
        values: Vec<Vec<u8>>,
        order: Vec<u32>,
        /// How many values were read to answer, which is what the searching path drives to zero.
        reads: AtomicUsize,
    }

    impl rudb_vector::TextSource for Filed {
        fn len(&self) -> usize {
            self.values.len()
        }

        fn bytes_at(&self, index: usize) -> rudb_common::Result<Option<&[u8]>> {
            self.reads.fetch_add(1, Memory::Relaxed);
            Ok(self.values.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.values.iter().map(Vec::len).sum()
        }

        fn ranks(&self) -> Option<usize> {
            Some(self.order.len())
        }

        fn compare_rank(&self, rank: usize, wanted: &[u8]) -> rudb_common::Result<Ordering> {
            // No read counted, because a format that keeps the start of each value beside its rank
            // settles a probe without going near the payload, and that is the case being tested.
            Ok(self.values[self.order[rank] as usize].as_slice().cmp(wanted))
        }

        fn code_at_rank(&self, rank: usize) -> rudb_common::Result<u32> {
            Ok(self.order[rank])
        }
    }

    /// A dictionary column over values that came out of a file with their sorted order, beside the
    /// same rows written out flat so the two can be compared.
    fn filed(words: &[&str], codes: Vec<u32>) -> (Vector, Vector, Arc<Filed>) {
        let values: Vec<Vec<u8>> = words.iter().map(|text| text.as_bytes().to_vec()).collect();
        let mut order = (0..values.len() as u32).collect::<Vec<_>>();
        order.sort_by(|&left, &right| values[left as usize].cmp(&values[right as usize]));
        let source = Arc::new(Filed { values, order, reads: AtomicUsize::new(0) });
        let dictionary = Arc::new(
            Vector::external_text(LogicalType::Varchar, Arc::clone(&source) as Arc<_>)
                .expect("a filed vector"),
        );
        let flat = Vector::from_values(
            LogicalType::Varchar,
            &codes
                .iter()
                .map(|&code| Value::Varchar(words[code as usize].into()))
                .collect::<Vec<_>>(),
        )
        .expect("the same rows written out");
        let column = Vector::stable_dictionary(codes, dictionary).expect("codes are in range");
        (column, flat, source)
    }

    /// The case the searching path exists for. The list is found in the dictionary once for the
    /// whole query and the rows are then codes against codes, so the answer is the answer the flat
    /// column gives and the payload is never touched.
    #[test]
    fn a_sorted_dictionary_is_searched_once_and_no_value_is_read() {
        let (column, flat, source) =
            filed(&["AIR", "MAIL", "RAIL", "SHIP", "TRUCK"], vec![1, 0, 3, 4, 1, 2, 3]);
        let list = [Value::Varchar("MAIL".into()), Value::Varchar("SHIP".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        assert_eq!(over(&column, &list, true), over(&flat, &list, true));
        assert_eq!(source.reads.load(Memory::Relaxed), 0);
    }

    /// A list the dictionary holds none of, which the search settles for the whole column without
    /// looking at a single code.
    #[test]
    fn a_list_the_dictionary_does_not_hold_is_false_everywhere() {
        let (column, flat, _) = filed(&["AIR", "MAIL", "SHIP"], vec![0, 1, 2, 1]);
        let list = [Value::Varchar("BOAT".into()), Value::Varchar("CART".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        assert_eq!(over(&column, &list, false), vec![Value::Boolean(false); 4]);
    }

    /// Half in and half out, which is the case a search that stopped at the first miss would get
    /// wrong, and the nulls of the column on top of it.
    #[test]
    fn a_list_the_dictionary_holds_some_of_answers_what_the_flat_column_answers() {
        let (column, flat, _) = filed(&["AIR", "MAIL", "SHIP"], vec![0, 1, 2, 1, 0]);
        let list = [Value::Varchar("MAIL".into()), Value::Varchar("BOAT".into())];
        assert_eq!(over(&column, &list, false), over(&flat, &list, false));
        let holed = column
            .with_validity(rudb_vector::Validity::from_run(&[true, false, true, true, false]));
        let holed_flat =
            flat.with_validity(rudb_vector::Validity::from_run(&[true, false, true, true, false]));
        assert_eq!(over(&holed, &list, false), over(&holed_flat, &list, false));
    }

    #[test]
    fn a_list_says_how_many_distinct_values_it_holds() {
        let list = [Value::Integer(1), Value::Integer(1), Value::Integer(2), Value::Null];
        let members = Members::of(&list, false).expect("this list folds");
        assert_eq!(members.len(), 2);
        assert!(!members.is_empty());
    }
}
