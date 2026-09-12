//! Turning a vector of flags into the rows it keeps.
//!
//! This is the other half of a filter. The comparison kernel produces a vector of booleans fast, and
//! then something has to turn that vector into the positions that survived, which is what an
//! operator hands downstream. Reading the flags back out one [`rudb_common::Value`] at a time
//! undoes the comparison kernel's work and then some: on a two million row table a comparison that
//! costs under a nanosecond a row was followed by a read that cost twenty seven.
//!
//! # Why the loop has no branch in it
//!
//! The obvious loop is `if kept { push(index) }`, and the branch in it is unpredictable by
//! construction. A filter that keeps every row or no rows predicts perfectly and is also a filter
//! nobody needed; the filters that matter keep some rows, and which rows is exactly the thing the
//! data decides rather than the code. A mispredict is somewhere between fifteen and twenty cycles,
//! so at thirty percent selectivity the branch alone can cost more than everything else in the loop.
//!
//! So every row writes its own index at the current length and only a row that is kept moves the
//! length on. The write is unconditional and lands in the same cache line most of the time, the
//! addition is of a zero or a one, and there is no branch for a predictor to get wrong. That is why
//! [`rudb_vector::Selection::from_indices`] exists: the buffer is filled and counted here and handed
//! over whole, rather than being pushed into one position at a time with a capacity check per row.
//!
//! A conjunct that is not the first one does not start from a range of rows, it starts from what
//! the conjuncts before it left, which is what [`refine`] is for. Most conjuncts never get here at
//! all because [`crate::compare::refine`] threads the selection into the comparison itself and
//! produces the rows directly, and this one catches the conjuncts that are not comparisons.
//!
//! Three valued logic is what makes this a kernel rather than a line. A filter keeps a row when the
//! predicate is true, and null is not true, which is what makes `WHERE x <> 5` leave out the rows
//! where `x` is null. So a row is kept when its flag is set and its validity bit is set, and the
//! second half of that is the reason the null path reads a word of the mask at a time rather than
//! asking the vector per row.

use rudb_common::{Error, LogicalType, Result};
use rudb_vector::{Data, Form, Selection, Validity, Vector};

use crate::fallback::{self, Kernel};
use crate::logic::is_true;
use crate::shape::{identity, nulls_of};

/// The first `rows` positions of `flags` where the flag is true and not null.
///
/// A vector that is not boolean, or is in a form with no loop here, falls through to reading it a
/// value at a time and records itself in [`crate::fallback`]. The answer is the same either way.
#[must_use]
pub fn selection(flags: &Vector, rows: usize) -> Selection {
    let rows = rows.min(flags.len());
    if let Some(kept) = swept(flags, rows) {
        return kept;
    }
    // One vector in, so its form goes in both halves of the report rather than leaving a column of
    // zeros next to every row of it.
    fallback::record(Kernel::Select, flags.form(), flags.form());
    Selection::from_predicate(rows, |index| is_true(&flags.value_at(index)))
}

/// The rows of `kept` whose flag is true and not null.
///
/// [`selection`] for a conjunct that is not the first one. A predicate is threaded through the
/// comparison kernel where it can be, because [`crate::compare::refine`] reads only the rows it is
/// given and never builds a flag vector at all, and through this where it cannot: a conjunct that is
/// a bare boolean column, a function call or a nested `OR` produces flags over the whole chunk and
/// then has to be intersected with what the conjuncts before it left. The second one is worth having
/// because a predicate with one awkward conjunct in it would otherwise put every conjunct back on
/// the unthreaded path.
///
/// # Errors
///
/// If a position in `kept` is past the end of `flags`.
pub fn refine(flags: &Vector, kept: &Selection) -> Result<Selection> {
    if kept.indices().iter().any(|&row| row as usize >= flags.len()) {
        return Err(Error::internal(format!(
            "a selection past the end of a {} row vector",
            flags.len()
        )));
    }
    if kept.is_empty() {
        return Ok(Selection::empty());
    }
    if let Some(narrowed) = swept_within(flags, kept) {
        return Ok(narrowed);
    }
    fallback::record(Kernel::Select, flags.form(), flags.form());
    let mut out = Vec::with_capacity(kept.len());
    // row at a time: the path recorded on the line above, for a flag vector in a form with no loop
    // here, reading only the rows the conjuncts before this one kept.
    for &row in kept.indices() {
        if is_true(&flags.value_at(row as usize)) {
            out.push(row);
        }
    }
    Ok(Selection::from_indices(out))
}

fn swept_within(flags: &Vector, kept: &Selection) -> Option<Selection> {
    if *flags.logical_type() != LogicalType::Boolean {
        return None;
    }
    match flags.form() {
        Form::Constant => {
            Some(if is_true(flags.constant_value()?) { kept.clone() } else { Selection::empty() })
        }
        Form::Flat => {
            let Data::Bool(values) = flags.data()? else {
                return None;
            };
            if values.len() < flags.len() {
                return None;
            }
            Some(picked_within(values, identity, kept.indices(), &nulls_of(flags)))
        }
        Form::Dictionary | Form::Rle => {
            let (codes, inner) = flags.positions()?;
            if codes.len() < flags.len() {
                return None;
            }
            let Data::Bool(values) = inner.data()? else {
                return None;
            };
            Some(picked_within(values, |row| codes[row] as usize, kept.indices(), &nulls_of(flags)))
        }
        _ => None,
    }
}

/// The same branchless loop as [`picked`], over the rows a selection names rather than over a range.
///
/// The validity is read a bit at a time here where [`picked`] reads a word at a time, because the
/// rows are scattered by construction and a word oriented loop would reread most of them.
fn picked_within<M: Fn(usize) -> usize>(
    values: &[bool],
    at: M,
    rows: &[u32],
    nulls: &Validity,
) -> Selection {
    let mut out = vec![0_u32; rows.len()];
    let mut count = 0;
    match nulls {
        Validity::AllValid => {
            for &row in rows {
                out[count] = row;
                count += usize::from(values[at(row as usize)]);
            }
        }
        Validity::AllInvalid => {}
        Validity::Mask(mask) => {
            for &row in rows {
                out[count] = row;
                // A single `&` rather than `&&`, because the short circuit would put back the
                // branch this whole loop is shaped to avoid.
                count += usize::from(mask.get(row as usize) & values[at(row as usize)]);
            }
        }
    }
    out.truncate(count);
    Selection::from_indices(out)
}

fn swept(flags: &Vector, rows: usize) -> Option<Selection> {
    if *flags.logical_type() != LogicalType::Boolean {
        return None;
    }
    // The indices are written as `u32`, which is what a selection holds. A vector is 1024 rows and
    // a row group is 122,880, so this is a bound the callers are nowhere near rather than a limit.
    if rows > u32::MAX as usize {
        return None;
    }
    match flags.form() {
        // One value decides the whole vector, and the answer is every row or no rows.
        Form::Constant => Some(if is_true(flags.constant_value()?) {
            Selection::identity(rows)
        } else {
            Selection::empty()
        }),
        Form::Flat => {
            let Data::Bool(values) = flags.data()? else {
                return None;
            };
            if values.len() < rows {
                return None;
            }
            Some(picked(values, identity, rows, &nulls_of(flags)))
        }
        Form::Dictionary | Form::Rle => {
            let (codes, inner) = flags.positions()?;
            if codes.len() < rows {
                return None;
            }
            let Data::Bool(values) = inner.data()? else {
                return None;
            };
            // Every code is inside the dictionary because `Vector::dictionary` checks that on the
            // way in, so the gather below indexes without a bound of its own.
            Some(picked(values, |index| codes[index] as usize, rows, &nulls_of(flags)))
        }
        _ => None,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the caller checked that the row count fits in a u32 before getting here"
)]
fn picked<M: Fn(usize) -> usize>(
    values: &[bool],
    at: M,
    rows: usize,
    nulls: &Validity,
) -> Selection {
    let mut out = vec![0_u32; rows];
    let mut kept = 0;
    match nulls {
        Validity::AllValid => {
            for index in 0..rows {
                out[kept] = index as u32;
                kept += usize::from(values[at(index)]);
            }
        }
        Validity::AllInvalid => {}
        Validity::Mask(mask) => {
            for start in (0..rows).step_by(64) {
                let word = mask.word(start / 64);
                for index in start..(start + 64).min(rows) {
                    out[kept] = index as u32;
                    let live = word >> (index - start) & 1 == 1;
                    // A single `&` rather than `&&`, because the short circuit would put back the
                    // branch this whole loop is shaped to avoid.
                    kept += usize::from(live & values[at(index)]);
                }
            }
        }
    }
    out.truncate(kept);
    Selection::from_indices(out)
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::*;

    fn flags(values: &[Value]) -> Vector {
        Vector::from_values(LogicalType::Boolean, values).expect("a vector of booleans")
    }

    const YES: Value = Value::Boolean(true);
    const NO: Value = Value::Boolean(false);

    /// The row at a time path, which is what the loop above has to agree with.
    fn oracle(vector: &Vector, rows: usize) -> Selection {
        Selection::from_predicate(rows, |index| is_true(&vector.value_at(index)))
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

    #[test]
    fn a_null_flag_is_not_a_true_flag() {
        let vector = flags(&[YES, Value::Null, NO, YES]);
        let kept = selection(&vector, 4);
        assert_eq!(kept.indices(), &[0, 3]);
        assert_eq!(kept, oracle(&vector, 4));
    }

    /// Every selectivity from nothing to everything, at four null densities, in both forms that
    /// have a loop, against reading the flags a value at a time.
    #[test]
    fn the_rows_kept_are_the_rows_the_row_at_a_time_path_keeps() {
        let mut rng = Rng(0x5eed_ca11_ab1e_0005);
        for nulls in [0_usize, 8, 3, 1] {
            for share in [0_u64, 1, 16, 50, 84, 99, 100] {
                let values: Vec<Value> = (0..251)
                    .map(|index| {
                        if nulls > 0 && index % nulls == 0 {
                            Value::Null
                        } else {
                            Value::Boolean(rng.next() % 100 < share)
                        }
                    })
                    .collect();
                let vector = flags(&values);
                let note = format!("{share} percent true, one null in {nulls}");
                assert_eq!(selection(&vector, 251), oracle(&vector, 251), "{note}, flat");
                let codes: Vec<u32> = (0..251).map(|index| (index % 37) as u32).collect();
                let coded = Vector::dictionary(codes, vector).expect("codes are in range");
                assert_eq!(selection(&coded, 251), oracle(&coded, 251), "{note}, dictionary");
            }
        }
    }

    /// The rows of a selection the row at a time path keeps, which is what [`refine`] has to say.
    fn within(vector: &Vector, kept: &Selection) -> Selection {
        let mut out = Vec::new();
        // row at a time: the oracle the threaded loop is checked against, which is the whole
        // reason the row at a time path is kept rather than deleted.
        for &row in kept.indices() {
            if is_true(&vector.value_at(row as usize)) {
                out.push(row);
            }
        }
        Selection::from_indices(out)
    }

    /// Threading a selection through a flag vector is the rows a full pass would have kept that
    /// were still in play. Every selectivity, four null densities, both forms with a loop, and the
    /// four shapes of selection a conjunct chain actually reaches.
    #[test]
    fn a_threaded_selection_keeps_what_was_still_in_play_and_true() {
        let mut rng = Rng(0x5eed_ca11_ab1e_0006);
        let len = 251;
        let selections = [
            Selection::identity(len),
            Selection::from_indices((0..len as u32).filter(|row| row % 7 == 0).collect()),
            Selection::from_indices(vec![0, 1, 128, 250]),
            Selection::empty(),
        ];
        for nulls in [0_usize, 8, 3, 1] {
            for share in [0_u64, 1, 16, 50, 84, 99, 100] {
                let values: Vec<Value> = (0..len)
                    .map(|index| {
                        if nulls > 0 && index % nulls == 0 {
                            Value::Null
                        } else {
                            Value::Boolean(rng.next() % 100 < share)
                        }
                    })
                    .collect();
                let vector = flags(&values);
                let codes: Vec<u32> = (0..len).map(|index| (index % 37) as u32).collect();
                let coded = Vector::dictionary(codes, vector.clone()).expect("codes are in range");
                let all = Vector::constant(LogicalType::Boolean, YES, len);
                for kept in &selections {
                    let note = format!("{share} percent true, one null in {nulls}");
                    let threaded = refine(&vector, kept).expect("in range");
                    assert_eq!(threaded, within(&vector, kept), "{note}, flat");
                    assert_eq!(
                        refine(&coded, kept).expect("in range"),
                        within(&coded, kept),
                        "{note}, dictionary"
                    );
                    // A constant true keeps everything that was in play and reads nothing.
                    assert_eq!(refine(&all, kept).expect("in range"), *kept, "{note}, constant");
                    // And what a threaded pass keeps is always a subset of what a full pass does.
                    let full = selection(&vector, len);
                    assert!(
                        threaded.indices().iter().all(|row| full.indices().contains(row)),
                        "{note}, threaded is within the full pass"
                    );
                }
            }
        }
    }

    #[test]
    fn a_threaded_selection_past_the_end_is_caught() {
        let vector = flags(&[YES, YES]);
        let past = Selection::from_indices(vec![0, 2]);
        let error = refine(&vector, &past).expect_err("out of range");
        assert!(error.message().contains("2 row vector"), "{error}");
    }

    #[test]
    fn a_constant_is_answered_without_a_loop_and_a_non_boolean_is_not_answered_at_all() {
        fallback::reset();
        let all = Vector::constant(LogicalType::Boolean, YES, 500);
        assert_eq!(selection(&all, 500), Selection::identity(500));
        let none = Vector::constant(LogicalType::Boolean, Value::Null, 500);
        assert!(selection(&none, 500).is_empty());
        assert_eq!(fallback::count(Kernel::Select, Form::Constant, Form::Constant), 0);

        // Nothing binds a filter to a non boolean, and if something did it would be wrong rather
        // than fast, so the loop refuses it and the value at a time path decides.
        let numbers = Vector::from_values(LogicalType::Integer, &[Value::Integer(1)])
            .expect("a vector of integers");
        assert!(selection(&numbers, 1).is_empty());
        assert_eq!(fallback::count(Kernel::Select, Form::Flat, Form::Flat), 1);
        fallback::reset();
    }

    /// Fewer rows than the vector holds, which is what a partly filled chunk is.
    #[test]
    fn only_the_rows_asked_for_are_looked_at() {
        let vector = flags(&[YES, YES, YES, YES]);
        assert_eq!(selection(&vector, 2).indices(), &[0, 1]);
        // And more rows than there are is the vector's length, not a panic.
        assert_eq!(selection(&vector, 9).indices(), &[0, 1, 2, 3]);
    }
}
