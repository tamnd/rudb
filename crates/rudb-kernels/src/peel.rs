//! Answering a predicate once per distinct value rather than once per row.
//!
//! A text column read out of a native file arrives as codes into one dictionary that covers the
//! whole table, and the same dictionary arrives with every chunk of it. So a predicate over that
//! column asks the same question about the same string over and over. `hits` has a million rows of
//! `Referer` and about four hundred thousand distinct ones, and every `WHERE Referer <> ''` in
//! ClickBench compares a million strings to answer four hundred thousand questions.
//!
//! Peeling the dictionary off and deciding on the values under it is the name Velox gives this.
//! DuckDB gets the same effect by letting a dictionary vector travel through an expression instead
//! of flattening it at the first operator. The two things that make it work here are that the
//! dictionary is shared as an `Arc`, so the same one is recognised from one chunk to the next by
//! its pointer, and that the memo is owned by something built once for the query, so it outlives
//! the chunk that filled it.
//!
//! The memo fills lazily rather than in one pass over the dictionary, because a chunk of two
//! thousand rows points at two thousand codes of four hundred thousand. A whole scan does reach
//! most of them in the end, but a scan behind a selective filter does not, and an eager pass would
//! be deciding for values nobody asked about.
//!
//! # What may be peeled
//!
//! Anything pure and per value. The answer for a code has to depend on the value under that code
//! and on nothing else, which rules out a predicate that reads the row number or another column.
//! That is the whole contract and it is the caller's to keep.
//!
//! # One peel, one question
//!
//! A [`Peel`] holds the answers to one question about one dictionary. The caller owns it from
//! somewhere that is built per query node, so the question cannot change under it, and the
//! dictionary is checked by pointer on every chunk so a second dictionary is declined rather than
//! answered from the first one's memo.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use rudb_common::{Error, Result};
use rudb_vector::Vector;

/// A memo of one predicate's answers over the values of one dictionary.
#[derive(Debug, Default)]
pub(crate) struct Peel {
    answers: OnceLock<Answers>,
}

#[derive(Debug)]
struct Answers {
    /// The dictionary these answers are about, recognised by pointer rather than by value.
    dictionary: Arc<Vector>,
    /// One per dictionary entry: zero for undecided, one for false and two for true.
    ///
    /// Atomic because the chunks of one column run on several threads and all of them fill the
    /// same memo. Relaxed is enough: two threads that decide the same code write the same byte,
    /// since the predicate is pure, so there is nothing for an ordering to protect.
    decided: Vec<AtomicU8>,
}

impl Peel {
    /// The predicate's answer for every slot, deciding once per distinct code.
    ///
    /// `column` is the vector the rows live in, `len` is how many slots the caller wants and `map`
    /// turns a slot into a row of the column, so this serves both a whole chunk and the rows an
    /// earlier conjunct kept. `decide` is handed the dictionary and a code and answers for the
    /// value under it.
    ///
    /// `None` means there is nothing to peel and the caller should do what it did before: either
    /// the column is not a dictionary that shares its values, or this memo was built for a
    /// different dictionary, which happens when one query node sees two columns.
    pub(crate) fn answer<M, D>(
        &self,
        column: &Vector,
        len: usize,
        map: M,
        decide: D,
    ) -> Option<Result<Vec<bool>>>
    where
        M: Fn(usize) -> usize,
        D: Fn(&Vector, usize) -> Result<bool>,
    {
        let (codes, dictionary) = column.shared_dictionary_parts()?;
        let answers = self.answers.get_or_init(|| Answers {
            dictionary: Arc::clone(dictionary),
            decided: (0..dictionary.len()).map(|_| AtomicU8::new(0)).collect(),
        });
        if !Arc::ptr_eq(&answers.dictionary, dictionary) {
            return None;
        }
        Some(answers.run(dictionary, codes, len, map, decide))
    }
}

impl Answers {
    fn run<M, D>(
        &self,
        dictionary: &Vector,
        codes: &[u32],
        len: usize,
        map: M,
        decide: D,
    ) -> Result<Vec<bool>>
    where
        M: Fn(usize) -> usize,
        D: Fn(&Vector, usize) -> Result<bool>,
    {
        let mut out = Vec::with_capacity(len);
        // row at a time: the loop is the point. Every row is one load of its code and one load of
        // the byte that code was already decided to, and only a code nobody has asked about yet
        // reaches the predicate.
        for slot in 0..len {
            let code = *codes
                .get(map(slot))
                .ok_or_else(|| Error::internal("a peeled row is past the end of its codes"))?;
            let state = self.decided.get(code as usize).ok_or_else(|| {
                Error::internal("a peeled code is past the end of its dictionary")
            })?;
            let mut held = state.load(Ordering::Relaxed);
            if held == 0 {
                held = u8::from(decide(dictionary, code as usize)?) + 1;
                state.store(held, Ordering::Relaxed);
            }
            out.push(held == 2);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use rudb_common::{LogicalType, Value};

    use super::*;

    fn letters(values: &[&str]) -> Vector {
        let values: Vec<Value> = values.iter().map(|text| Value::Varchar((*text).into())).collect();
        Vector::from_values(LogicalType::Varchar, &values).expect("a vector of text")
    }

    fn holds(dictionary: &Vector, code: usize) -> Result<bool> {
        Ok(dictionary.try_bytes_at(code)?.is_some_and(|bytes| bytes.starts_with(b"a")))
    }

    #[test]
    fn a_peel_decides_once_per_distinct_code_and_reads_the_memo_after_that() {
        let values = Arc::new(letters(&["apple", "pear", "avocado"]));
        let column = Vector::stable_dictionary(vec![0, 1, 2, 1, 0, 0], values).expect("in range");
        let peel = Peel::default();
        let calls = AtomicUsize::new(0);
        let answers = peel
            .answer(
                &column,
                6,
                |slot| slot,
                |dictionary, code| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    holds(dictionary, code)
                },
            )
            .expect("a shared dictionary is peelable")
            .expect("the predicate answers");
        assert_eq!(answers, [true, false, true, false, true, true]);
        assert_eq!(calls.load(Ordering::Relaxed), 3, "six rows over three distinct values");
    }

    /// The memo has to survive the chunk, because that is the only thing that makes it worth
    /// building. A second chunk over the same dictionary asks the predicate nothing.
    #[test]
    fn a_second_chunk_over_the_same_dictionary_asks_the_predicate_nothing() {
        let values = Arc::new(letters(&["apple", "pear"]));
        let first = Vector::stable_dictionary(vec![0, 1], Arc::clone(&values)).expect("in range");
        let second = Vector::stable_dictionary(vec![1, 1, 0], values).expect("in range");
        let peel = Peel::default();
        let calls = AtomicUsize::new(0);
        let count = |column: &Vector, len: usize| {
            peel.answer(
                column,
                len,
                |slot| slot,
                |dictionary, code| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    holds(dictionary, code)
                },
            )
            .expect("peelable")
            .expect("answers")
        };
        assert_eq!(count(&first, 2), [true, false]);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(count(&second, 3), [false, false, true]);
        assert_eq!(calls.load(Ordering::Relaxed), 2, "the second chunk decided nothing new");
    }

    /// One query node that sees a second dictionary declines rather than answering the new codes
    /// out of the old one's memo, which would be a wrong answer rather than a slow one.
    #[test]
    fn a_different_dictionary_is_declined_rather_than_answered_from_the_first_ones_memo() {
        let peel = Peel::default();
        let first = Vector::stable_dictionary(vec![0], Arc::new(letters(&["apple"]))).expect("one");
        let second = Vector::stable_dictionary(vec![0], Arc::new(letters(&["pear"]))).expect("one");
        assert!(peel.answer(&first, 1, |slot| slot, holds).is_some());
        assert!(peel.answer(&second, 1, |slot| slot, holds).is_none());
    }

    #[test]
    fn a_column_that_is_not_a_shared_dictionary_has_nothing_to_peel() {
        let flat = letters(&["apple", "pear"]);
        assert!(Peel::default().answer(&flat, 2, |slot| slot, holds).is_none());
    }
}
