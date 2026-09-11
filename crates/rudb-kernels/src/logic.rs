//! `AND` and `OR` over three values.
//!
//! The two rules that matter are that `FALSE AND NULL` is false and `TRUE OR NULL` is true. Both
//! are the reason a conjunction cannot be evaluated by treating null as false and then fixing it up
//! afterwards: `NULL AND FALSE` is false, `NULL AND TRUE` is null, and no single substitution for
//! null gets both.
//!
//! A conjunction here is flat, over two or more children, because that is the shape the binder
//! produces and the shape filter pushdown wants. Evaluating a flat one is a fold with an early
//! answer, which is also why the null case is cheap: once a false has been seen in an `AND` nothing
//! any other child says can change the result.
//!
//! # How the vectorized path is put together
//!
//! The fold above is stated per row, and per row is exactly what it must not be. The shape that
//! runs fast is the same fold turned inside out: one pass per child over all the rows, carrying two
//! boolean runs rather than one three-valued answer.
//!
//! The first run says a child has already produced the value that decides the answer, which is
//! false for `AND` and true for `OR`. The second says a child was null. A row where the first is set
//! is the deciding value and is not null, however many nulls it saw. A row where only the second is
//! set is null. A row where neither is set is the other value. That is the whole of three-valued
//! logic with no branch in it, because both runs are accumulated with `or` rather than tested.
//!
//! Writing it that way also makes the number of children free. Ten conjuncts are ten passes over a
//! run of bytes that stays in L1, rather than ten `Value` constructions per row, and the pass for a
//! child whose validity is `AllInvalid` does not look at the data at all: every row it can speak to
//! is unknown, so it sets the second run and returns.
//!
//! A constant child does not get a pass. It either decides every row, in which case one `fill` says
//! so, or it decides none of them, in which case it is dropped. `WHERE a AND true` costs nothing
//! after binding, which matters because that is the shape a pushed down filter with one conjunct
//! removed actually has.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Buffer, Data, Form, Validity, Vector};

use crate::fallback::{self, Kernel};
use crate::shape::{identity, nulls_of};

/// Which connective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Connective {
    /// `AND`.
    And,
    /// `OR`.
    Or,
}

/// Combines two or more boolean vectors.
///
/// The children are anything that hands back a [`Vector`] by reference, which is `&[Vector]` for a
/// caller holding a list it built and `&[&Vector]` for one whose operands are somewhere else. The
/// evaluator at layer twelve is the second kind: its operands are slots in a scratch array and
/// columns of the chunk it was handed, and a signature that demanded a `Vec<Vector>` would make it
/// copy every column of every conjunct on every chunk to satisfy the type rather than the work.
///
/// # Errors
///
/// If there are no children, if they are not all the same length, or if one of them is not boolean.
pub fn combine<V: AsRef<Vector>>(op: Connective, children: &[V]) -> Result<Vector> {
    let first = children
        .first()
        .map(AsRef::as_ref)
        .ok_or_else(|| Error::internal("a conjunction with no children"))?;
    let rows = first.len();
    for (at, child) in children.iter().enumerate() {
        if child.as_ref().len() != rows {
            return Err(Error::internal(format!(
                "child {at} of a conjunction is {} rows and child 0 is {rows}",
                child.as_ref().len()
            )));
        }
    }
    if let Some(vector) = folded(op, children, rows) {
        return Ok(vector);
    }
    let left = first.form();
    fallback::record(Kernel::Logic, left, children.get(1).map_or(left, |c| c.as_ref().form()));
    let mut values = Vec::with_capacity(rows);
    // row at a time: the path recorded on the line above, which exists to be correct for a set of
    // forms `folded` does not cover and counts itself so that set shows up.
    for index in 0..rows {
        let mut answer = Some(matches!(op, Connective::And));
        for child in children {
            let held = match child.as_ref().value_at(index) {
                Value::Boolean(held) => Some(held),
                Value::Null => None,
                other => {
                    return Err(Error::internal(format!(
                        "a conjunction over a {} value",
                        other.logical_type()
                    )));
                }
            };
            answer = fold(op, answer, held);
        }
        values.push(match answer {
            Some(held) => Value::Boolean(held),
            None => Value::Null,
        });
    }
    Vector::from_values(LogicalType::Boolean, &values)
}

/// The vectorized fold, or `None` for a shape it does not handle.
///
/// The connective becomes a constant generic here and nowhere else. Which value decides the answer
/// is the only thing that differs between `AND` and `OR` in the loop below, and passing it as a
/// value would put a comparison against it inside the loop for something that cannot change while
/// the loop runs.
fn folded<V: AsRef<Vector>>(op: Connective, children: &[V], rows: usize) -> Option<Vector> {
    match op {
        Connective::And => fold_runs::<false, _>(children, rows),
        Connective::Or => fold_runs::<true, _>(children, rows),
    }
}

/// One pass per child, carrying the run that says decided and the run that says unknown.
///
/// `DOMINANT` is the value that ends the question for a row: false for `AND`, true for `OR`.
fn fold_runs<const DOMINANT: bool, V: AsRef<Vector>>(
    children: &[V],
    rows: usize,
) -> Option<Vector> {
    if rows == 0 {
        // What `from_values` builds from no values at all, which is the empty run rather than
        // `Data::Empty` and validity that normalizes to all valid. Written out rather than reached
        // by falling through so that an empty chunk does not show up in the fallback counters as a
        // form pair worth specializing.
        return Vector::flat(LogicalType::Boolean, Data::Bool(Buffer::new())).ok();
    }
    // A child that is not boolean is an error the row at a time path raises with the type in the
    // message, and it raises it only for rows that are not null, so the fast path cannot answer for
    // it at all. It hands the whole call back rather than guessing.
    if children.iter().any(|child| child.as_ref().logical_type() != &LogicalType::Boolean) {
        return None;
    }

    let mut decided = vec![false; rows];
    let mut unknown = vec![false; rows];
    let mut nullable = false;

    for child in children {
        let child = child.as_ref();
        let nulls = nulls_of(child);
        nullable |= nulls.has_nulls(rows);
        match child.form() {
            Form::Constant => match child.value_at(0) {
                Value::Boolean(held) if held == DOMINANT => decided.fill(true),
                Value::Boolean(_) => {}
                Value::Null => unknown.fill(true),
                _ => return None,
            },
            Form::Flat => {
                let Some(Data::Bool(values)) = child.data() else {
                    return None;
                };
                if values.len() < rows {
                    return None;
                }
                absorb::<DOMINANT, _>(values, identity, &nulls, &mut decided, &mut unknown);
            }
            Form::Dictionary => {
                let (codes, values) = child.dictionary_parts()?;
                let Some(Data::Bool(held)) = values.data() else {
                    return None;
                };
                if codes.len() < rows {
                    return None;
                }
                absorb::<DOMINANT, _>(
                    held,
                    |index| codes[index] as usize,
                    &nulls,
                    &mut decided,
                    &mut unknown,
                );
            }
            _ => return None,
        }
    }

    // All valid against all valid is the discriminant comparison the whole run was tracking, and it
    // skips this pass entirely rather than walking a bitmap that is going to say valid every time.
    let validity = if nullable {
        // Packed a word at a time from the two runs, because building it a bit at a time is a read
        // modify write per row that depends on the row before it.
        let live: Vec<bool> =
            decided.iter().zip(&unknown).map(|(&hit, &null)| hit || !null).collect();
        Validity::from_run(&live)
    } else {
        Validity::AllValid
    };
    let data = if DOMINANT {
        decided
    } else {
        // The filler under a null has to be the false that `push_value` writes, and a row that is
        // unknown is a row nothing decided, so the same expression produces both.
        decided.iter().zip(&unknown).map(|(&hit, &null)| !(hit | null)).collect()
    };
    Some(Vector::flat(LogicalType::Boolean, Data::Bool(data.into())).ok()?.with_validity(validity))
}

/// Folds one child into the two runs.
///
/// `at` is a generic parameter rather than a function pointer, so that the flat pass and the
/// dictionary pass are two monomorphizations with the indexing inlined into each rather than one
/// loop with an indirect call in it. That difference measured at ten nanoseconds a row in
/// `scalar.rs` and there is no reason to rediscover it here.
fn absorb<const DOMINANT: bool, M: Fn(usize) -> usize>(
    values: &[bool],
    at: M,
    nulls: &Validity,
    decided: &mut [bool],
    unknown: &mut [bool],
) {
    match nulls {
        Validity::AllValid => {
            for (index, slot) in decided.iter_mut().enumerate() {
                *slot |= values[at(index)] == DOMINANT;
            }
        }
        // Every row this child could speak to is unknown, so the data is not read at all.
        Validity::AllInvalid => unknown.fill(true),
        Validity::Mask(mask) => {
            // Sixty four rows to a word, so the validity bits cost one load for the run rather
            // than a bounds check and a shift each.
            for (word_at, (hits, nulls)) in
                decided.chunks_mut(64).zip(unknown.chunks_mut(64)).enumerate()
            {
                let word = mask.word(word_at);
                let base = word_at * 64;
                for (bit, (hit, null)) in hits.iter_mut().zip(nulls.iter_mut()).enumerate() {
                    let valid = word >> bit & 1 == 1;
                    // Not `&&`, because a branch per row on the validity bit is the thing being
                    // removed, and the index is in range for a null row as much as for a live one.
                    *hit |= valid & (values[at(base + bit)] == DOMINANT);
                    *null |= !valid;
                }
            }
        }
    }
}

/// One step of the fold, where `None` is unknown.
///
/// The short circuit is on the value rather than on the position: a false anywhere in an `AND`
/// wins over an unknown that came before it, which is exactly the case a two-valued fold gets
/// wrong.
fn fold(op: Connective, left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match op {
        Connective::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Connective::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
    }
}

/// Whether a predicate keeps a row.
///
/// True keeps it, and false and null both drop it. That is `WHERE`'s rule and it is not `CHECK`'s,
/// which keeps a row whose constraint is unknown.
#[must_use]
pub fn is_true(value: &Value) -> bool {
    matches!(value, Value::Boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(values: &[Value]) -> Vector {
        Vector::from_values(LogicalType::Boolean, values).expect("booleans")
    }

    const TRUE: Value = Value::Boolean(true);
    const FALSE: Value = Value::Boolean(false);

    #[test]
    fn a_false_wins_an_and_even_against_an_unknown() {
        let result = combine(Connective::And, &[vector(&[Value::Null]), vector(&[FALSE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), FALSE);
    }

    #[test]
    fn a_true_wins_an_or_even_against_an_unknown() {
        let result = combine(Connective::Or, &[vector(&[Value::Null]), vector(&[TRUE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), TRUE);
    }

    #[test]
    fn an_unknown_survives_when_nothing_decides_it() {
        let result = combine(Connective::And, &[vector(&[Value::Null]), vector(&[TRUE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), Value::Null);
        let result = combine(Connective::Or, &[vector(&[Value::Null]), vector(&[FALSE])])
            .expect("two booleans");
        assert_eq!(result.value_at(0), Value::Null);
    }

    #[test]
    fn a_flat_conjunction_of_more_than_two_children_is_one_pass() {
        let result = combine(
            Connective::And,
            &[vector(&[TRUE]), vector(&[TRUE]), vector(&[TRUE]), vector(&[FALSE])],
        )
        .expect("four booleans");
        assert_eq!(result.value_at(0), FALSE);
    }

    #[test]
    fn a_where_clause_drops_the_rows_it_cannot_decide() {
        assert!(is_true(&TRUE));
        assert!(!is_true(&FALSE));
        assert!(!is_true(&Value::Null));
    }

    #[test]
    fn a_conjunction_with_no_children_is_caught() {
        let nothing: &[Vector] = &[];
        let error = combine(Connective::And, nothing).expect_err("nothing to combine");
        assert!(error.message().contains("no children"), "{error}");
    }

    /// The loop this file used to be, kept verbatim as the thing the fast path is checked against.
    ///
    /// It is the oracle rather than dead code. Every property test below runs both and compares
    /// whole vectors, so a disagreement about which rows are null, or about the filler stored under
    /// a null, is a failure and not something that has to be noticed by eye later.
    fn oracle(op: Connective, children: &[Vector]) -> Result<Vector> {
        let rows = children.first().map_or(0, Vector::len);
        let mut values = Vec::with_capacity(rows);
        for index in 0..rows {
            let mut answer = Some(matches!(op, Connective::And));
            for child in children {
                let held = match child.value_at(index) {
                    Value::Boolean(held) => Some(held),
                    Value::Null => None,
                    other => {
                        return Err(Error::internal(format!(
                            "a conjunction over a {} value",
                            other.logical_type()
                        )));
                    }
                };
                answer = fold(op, answer, held);
            }
            values.push(match answer {
                Some(held) => Value::Boolean(held),
                None => Value::Null,
            });
        }
        Vector::from_values(LogicalType::Boolean, &values)
    }

    fn agrees(op: Connective, children: &[Vector]) {
        let fast = combine(op, children);
        let slow = oracle(op, children);
        match (fast, slow) {
            (Ok(fast), Ok(slow)) => assert_eq!(fast, slow, "{op:?} over {children:?}"),
            (Err(fast), Err(slow)) => {
                assert_eq!(fast.message(), slow.message(), "{op:?} over {children:?}");
            }
            (fast, slow) => panic!("{op:?} over {children:?} gave {fast:?} and {slow:?}"),
        }
    }

    /// Reproducible noise. The seed is written down so a failure is a failure twice.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// A boolean vector of `rows` rows with one null in `nulls` when `nulls` is not zero.
    fn sample(rng: &mut Rng, rows: usize, nulls: u64) -> Vector {
        let values: Vec<Value> = (0..rows)
            .map(|_| {
                let draw = rng.next();
                if nulls > 0 && draw % nulls == 0 {
                    Value::Null
                } else {
                    Value::Boolean(draw % 2 == 0)
                }
            })
            .collect();
        vector(&values)
    }

    #[test]
    fn every_form_and_null_density_agrees_with_the_row_at_a_time_path() {
        let mut rng = Rng(0x5eed_1eaf_c0ff_ee01);
        let rows = 97;
        for op in [Connective::And, Connective::Or] {
            for nulls in [0, 2, 7] {
                let flat = sample(&mut rng, rows, nulls);
                let other = sample(&mut rng, rows, nulls);
                let third = sample(&mut rng, rows, nulls);

                // Flat against flat, which is what a scan produces.
                agrees(op, &[flat.clone(), other.clone()]);
                // A flat conjunction of more than two, which is what the binder produces.
                agrees(op, &[flat.clone(), other.clone(), third.clone()]);
                // One child on its own, which is what a filter with one conjunct is.
                agrees(op, std::slice::from_ref(&flat));

                // Every constant a boolean column can be, on both sides.
                for held in [TRUE, FALSE, Value::Null] {
                    let constant = Vector::constant(LogicalType::Boolean, held, rows);
                    agrees(op, &[flat.clone(), constant.clone()]);
                    agrees(op, &[constant.clone(), flat.clone()]);
                    agrees(op, &[constant.clone(), flat.clone(), other.clone()]);
                }

                // A dictionary, whose nulls live in the vector it points at rather than in its own
                // validity, which is the wrong answer this crate is most likely to produce.
                let dictionary = Vector::dictionary(
                    (0..rows)
                        .map(|index| u32::try_from(index % 3).expect("a code under three"))
                        .collect(),
                    vector(&[TRUE, FALSE, Value::Null]),
                )
                .expect("three codes into three values");
                agrees(op, &[dictionary.clone(), flat.clone()]);
                agrees(op, &[flat.clone(), dictionary.clone()]);
                agrees(op, &[dictionary.clone(), dictionary.clone()]);
            }
        }
    }

    #[test]
    fn a_child_that_is_all_null_still_lets_a_decided_row_through() {
        // The pass for an all invalid child does not read its data at all, and the risk in that is
        // forgetting that a false elsewhere still decides the row. `NULL AND FALSE` is false.
        let rows = 8;
        let gone = Vector::constant(LogicalType::Boolean, Value::Null, rows);
        let mixed = vector(&[TRUE, FALSE, TRUE, FALSE, TRUE, FALSE, TRUE, FALSE]);
        agrees(Connective::And, &[gone.clone(), mixed.clone()]);
        agrees(Connective::Or, &[gone.clone(), mixed.clone()]);
        let result = combine(Connective::And, &[gone, mixed]).expect("two booleans");
        assert_eq!(result.value_at(0), Value::Null);
        assert_eq!(result.value_at(1), FALSE);
    }

    #[test]
    fn an_empty_conjunction_of_empty_children_is_an_empty_answer() {
        let empty = vector(&[]);
        agrees(Connective::And, &[empty.clone(), empty.clone()]);
        agrees(Connective::Or, &[empty.clone(), empty]);
    }

    #[test]
    fn a_child_that_is_not_boolean_is_still_caught_by_name() {
        let numbers = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(0), Value::Integer(3)],
        )
        .expect("integers");
        let error = combine(Connective::And, &[vector(&[TRUE, TRUE, TRUE]), numbers])
            .expect_err("a conjunction over integers");
        assert!(error.message().contains("conjunction over"), "{error}");
        assert!(error.message().contains("INTEGER"), "{error}");
    }

    #[test]
    fn a_form_pair_with_no_loop_is_still_right_and_says_so() {
        // The counters are per thread in a test build, so this reads its own and nothing else's.
        let before = fallback::count(Kernel::Logic, Form::Sequence, Form::Flat);
        let rows = 4;
        let ids = Vector::sequence(0, 1, rows);
        let flat = vector(&[TRUE, FALSE, TRUE, FALSE]);
        // A sequence is a run of integers whatever anybody wants it to be, so this is the error
        // path, and it has to be the same error the row at a time path raises.
        let error = combine(Connective::And, &[ids, flat]).expect_err("a conjunction over bigints");
        assert!(error.message().contains("conjunction over"), "{error}");
        assert!(fallback::count(Kernel::Logic, Form::Sequence, Form::Flat) > before);
    }

    #[test]
    fn a_children_length_mismatch_names_the_child_that_is_wrong() {
        let error = combine(Connective::And, &[vector(&[TRUE, TRUE]), vector(&[TRUE])])
            .expect_err("two lengths");
        assert!(error.message().contains("child 1"), "{error}");
    }
}
