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
//!
//! # When the dictionary is sorted, do not peel at all
//!
//! A peel still has to read every distinct value the rows use, and for `Referer` on `hits` that is
//! four hundred thousand reads out of the file. A dictionary that comes with its sorted order can
//! do better than that for the one question that matters most, which is whether a value equals a
//! literal, because the answer is a binary search: about nineteen probes for the whole query, and
//! after it the filter is an integer compare against one code with no memo to consult.
//!
//! [`Lookup`] is that search, memoized the same way and for the same reason. A probe asks the
//! values how they compare rather than asking them for bytes, which is what lets a format that
//! keeps the start of each value beside its rank answer nineteen probes out of nineteen without
//! going near the payload. That matters more than it sounds: the values a binary search lands on
//! are scattered all over the column, so nineteen reads of them is nineteen different blocks of
//! the file, which is more than the peel behind a selective filter would have read.
//!
//! It only answers equality. A `LIKE`, a regular expression or any other scalar function still
//! needs the peel, and always will, because no ordering of the values tells you which of them
//! match a pattern.

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

/// Where one literal sits in a dictionary that came with its sorted order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    /// The dictionary holds the literal under this code, so a row matches exactly when its code is
    /// this one.
    At(u32),
    /// The dictionary does not hold the literal at all, so no row of this column matches it. This
    /// is the case a whole scan can sometimes be skipped on, and it costs the same search to find.
    Absent,
}

/// The result of searching one dictionary for one literal, kept for the life of a query node.
#[derive(Debug, Default)]
pub(crate) struct Lookup {
    memo: OnceLock<Searched>,
}

#[derive(Debug)]
struct Searched {
    /// The dictionary this was searched in, recognised by pointer the way [`Peel`] does it.
    dictionary: Arc<Vector>,
    found: Found,
}

impl Lookup {
    /// Where `wanted` sits in `column`'s dictionary, searched once and remembered.
    ///
    /// `None` means there is nothing to search and the caller should do what it did before: the
    /// column is not a dictionary that shares its values, or the values did not arrive with a
    /// sorted order, or this was built against a different dictionary.
    ///
    /// A failed read is returned rather than remembered, so a caller that retries gets the error
    /// again rather than a wrong answer cached from a half finished search.
    pub(crate) fn find(&self, column: &Vector, wanted: &[u8]) -> Option<Result<Found>> {
        let (_, dictionary) = column.shared_dictionary_parts()?;
        if let Some(memo) = self.memo.get() {
            return Arc::ptr_eq(&memo.dictionary, dictionary).then_some(Ok(memo.found));
        }
        let ranks = dictionary.ranks()?;
        let found = match search(dictionary, ranks, wanted) {
            Ok(found) => found,
            Err(error) => return Some(Err(error)),
        };
        // Two threads that get here at once do the same search and set the same answer, and the
        // one that loses the race drops its own copy of it. Both return what they found.
        let _ = self.memo.set(Searched { dictionary: Arc::clone(dictionary), found });
        Some(Ok(found))
    }
}

/// The code of `wanted` in a dictionary, by binary search over its sorted order.
///
/// The search is here and the comparison is in the source on purpose. What the search does is the
/// same for every format, and it is short enough to read in one go and to swap for something else.
/// What one probe costs is entirely up to whoever wrote the file, and a format that keeps the start
/// of each value beside its rank answers almost every probe without reading a value at all. Asking
/// the source to compare rather than asking it for a position is what leaves room for that.
pub(crate) fn search(dictionary: &Vector, ranks: usize, wanted: &[u8]) -> Result<Found> {
    let mut low = 0;
    let mut high = ranks;
    while low < high {
        let middle = low + (high - low) / 2;
        match dictionary.compare_rank(middle, wanted)? {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => {
                return Ok(Found::At(dictionary.code_at_rank(middle)?));
            }
        }
    }
    Ok(Found::Absent)
}

/// How many of a dictionary's values sort before `wanted`, and whether one of them is `wanted`.
///
/// [`search`] answers where a literal is and this answers where it would go, which is what an
/// inequality needs and an equality does not. The search itself belongs to the source rather than to
/// this crate, because a source that is asked the same question twice is allowed to answer the
/// second one out of the first, and a loop here could not let it. See [`crate::TextSource::below`].
///
/// What the count is for: with `below` values under the literal and `equal` saying whether the
/// literal itself is in there, a value of rank `r` is under the literal exactly when `r < below` and
/// under or equal to it exactly when `r < below + equal`. Those two boundaries answer all four
/// inequalities between them, and neither of them reads a value.
pub(crate) fn below(dictionary: &Vector, ranks: usize, wanted: &[u8]) -> Result<(usize, bool)> {
    dictionary.below(ranks, wanted)
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

    /// Stands in for the values of a native column, which are reachable one at a time out of the
    /// file and which arrive with the sorted order the writer worked out. Counting the reads is
    /// the point, because the whole claim is that a search does a few of them and a peel does one
    /// for every distinct value.
    #[derive(Debug)]
    struct Filed {
        values: Vec<Vec<u8>>,
        /// Codes in sorted value order with the head of each value beside it, which is what the
        /// native format writes and what lets a probe answer without reading a value.
        order: Vec<(u64, u32)>,
        reads: AtomicUsize,
    }

    /// The first eight bytes of a value as an integer that sorts the way the bytes sort, which is
    /// what the native format stores per rank.
    fn head(bytes: &[u8]) -> u64 {
        let mut word = [0; 8];
        let take = bytes.len().min(8);
        word[..take].copy_from_slice(&bytes[..take]);
        u64::from_be_bytes(word)
    }

    impl Filed {
        /// `values` in the order the writer handed out codes, which is not sorted order.
        fn new(values: &[&str]) -> Self {
            let values: Vec<Vec<u8>> = values.iter().map(|text| text.as_bytes().to_vec()).collect();
            let mut order = (0..values.len() as u32)
                .map(|code| (head(&values[code as usize]), code))
                .collect::<Vec<_>>();
            order.sort_by(|&(_, left), &(_, right)| {
                values[left as usize].cmp(&values[right as usize])
            });
            Self { values, order, reads: AtomicUsize::new(0) }
        }

        fn at(&self, rank: usize) -> Result<(u64, u32)> {
            self.order
                .get(rank)
                .copied()
                .ok_or_else(|| Error::internal("a rank past the end of the order"))
        }
    }

    impl rudb_vector::TextSource for Filed {
        fn len(&self) -> usize {
            self.values.len()
        }

        fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.values.get(index).map(Vec::as_slice))
        }

        fn footprint(&self) -> usize {
            self.values.iter().map(Vec::len).sum()
        }

        fn ranks(&self) -> Option<usize> {
            Some(self.order.len())
        }

        fn compare_rank(&self, rank: usize, wanted: &[u8]) -> Result<std::cmp::Ordering> {
            let (found, code) = self.at(rank)?;
            let settled = found.cmp(&head(wanted));
            if settled != std::cmp::Ordering::Equal {
                return Ok(settled);
            }
            Ok(self.bytes_at(code as usize)?.unwrap_or_default().cmp(wanted))
        }

        fn code_at_rank(&self, rank: usize) -> Result<u32> {
            Ok(self.at(rank)?.1)
        }
    }

    /// A dictionary of `count` values named after their position, with codes handed out in an
    /// order that is nothing like sorted order, plus the source so a test can count its reads.
    fn filed(count: usize) -> (Vector, Arc<Filed>) {
        let spellings =
            (0..count).map(|at| format!("value-{:04}", (at * 7919) % count)).collect::<Vec<_>>();
        let source =
            Arc::new(Filed::new(&spellings.iter().map(String::as_str).collect::<Vec<_>>()));
        let values = Vector::external_text(LogicalType::Varchar, Arc::clone(&source) as Arc<_>)
            .expect("a filed vector");
        (values, source)
    }

    #[test]
    fn a_literal_is_found_in_a_sorted_dictionary_without_reading_every_value() {
        let (values, source) = filed(1024);
        let column = Vector::stable_dictionary(vec![3, 900, 3], Arc::new(values)).expect("codes");
        let found = Lookup::default()
            .find(&column, b"value-0700")
            .expect("a sorted dictionary can be searched")
            .expect("the search reads");
        let Found::At(code) = found else { panic!("the dictionary holds it") };
        assert_eq!(source.values[code as usize], b"value-0700");
        let reads = source.reads.load(Ordering::Relaxed);
        assert!(reads <= 11, "a search of 1024 values read {reads} of them");
    }

    /// The point of writing the start of each value beside its rank. Every probe of this search is
    /// settled by eight bytes the search already has, so the only value it reads is the one it
    /// found, and a literal the dictionary does not hold costs no reads at all.
    #[test]
    fn a_search_over_values_that_differ_early_reads_only_the_one_it_finds() {
        let source = Arc::new(Filed::new(&["cherry", "apple", "date", "banana"]));
        let values = Vector::external_text(LogicalType::Varchar, Arc::clone(&source) as Arc<_>)
            .expect("a filed vector");
        let column = Vector::stable_dictionary(vec![0, 1, 2, 3], Arc::new(values)).expect("codes");
        assert_eq!(
            Lookup::default().find(&column, b"cherry").expect("searchable").expect("read"),
            Found::At(0)
        );
        assert_eq!(source.reads.load(Ordering::Relaxed), 1, "only the value it found");
        assert_eq!(
            Lookup::default().find(&column, b"fig").expect("searchable").expect("read"),
            Found::Absent
        );
        assert_eq!(source.reads.load(Ordering::Relaxed), 1, "and nothing for the one it did not");
    }

    /// Two values that start the same way cannot be told apart by their first eight bytes, so the
    /// search has to read them, and the answer has to come out right anyway.
    #[test]
    fn values_that_share_their_first_eight_bytes_are_still_told_apart() {
        let source = Arc::new(Filed::new(&["prefixed-two", "prefixed-one", "prefixed-three"]));
        let values = Vector::external_text(LogicalType::Varchar, Arc::clone(&source) as Arc<_>)
            .expect("a filed vector");
        let column = Vector::stable_dictionary(vec![0, 1, 2], Arc::new(values)).expect("codes");
        for (wanted, expected) in [
            (&b"prefixed-one"[..], Found::At(1)),
            (b"prefixed-two", Found::At(0)),
            (b"prefixed-three", Found::At(2)),
            (b"prefixed-four", Found::Absent),
        ] {
            assert_eq!(
                Lookup::default().find(&column, wanted).expect("searchable").expect("read"),
                expected,
                "searching for {}",
                String::from_utf8_lossy(wanted)
            );
        }
    }

    #[test]
    fn a_literal_the_dictionary_does_not_hold_is_answered_absent() {
        let (values, _) = filed(64);
        let column = Vector::stable_dictionary(vec![0], Arc::new(values)).expect("codes");
        let found = Lookup::default().find(&column, b"nothing").expect("searchable").expect("read");
        assert_eq!(found, Found::Absent);
    }

    /// The empty string is the literal ten ClickBench queries filter on, and it is the one value
    /// most likely to sit at rank zero, so it is worth naming as its own case.
    #[test]
    fn the_empty_string_is_found_like_any_other_value() {
        let source = Arc::new(Filed::new(&["pear", "", "apple"]));
        let values = Vector::external_text(LogicalType::Varchar, source).expect("a filed vector");
        let column = Vector::stable_dictionary(vec![0, 1, 2], Arc::new(values)).expect("codes");
        assert_eq!(
            Lookup::default().find(&column, b"").expect("searchable").expect("read"),
            Found::At(1)
        );
    }

    #[test]
    fn a_second_chunk_over_the_same_dictionary_searches_nothing() {
        let (values, source) = filed(256);
        let values = Arc::new(values);
        let first = Vector::stable_dictionary(vec![0, 1], Arc::clone(&values)).expect("codes");
        let second = Vector::stable_dictionary(vec![2], Arc::clone(&values)).expect("codes");
        let lookup = Lookup::default();
        let found = lookup.find(&first, b"value-0100").expect("searchable").expect("read");
        let reads = source.reads.load(Ordering::Relaxed);
        assert!(reads > 0, "the first chunk did the search");
        assert_eq!(lookup.find(&second, b"value-0100").expect("searchable").expect("read"), found);
        assert_eq!(source.reads.load(Ordering::Relaxed), reads, "the second chunk read nothing");
    }

    /// Whatever a dictionary knows about itself, a second one is a different question, and
    /// answering it from the first one's search would be wrong rather than slow.
    #[test]
    fn a_search_is_not_reused_across_two_dictionaries() {
        let lookup = Lookup::default();
        let (first, _) = filed(8);
        let (second, _) = filed(8);
        let first = Vector::stable_dictionary(vec![0], Arc::new(first)).expect("codes");
        let second = Vector::stable_dictionary(vec![0], Arc::new(second)).expect("codes");
        assert!(lookup.find(&first, b"value-0000").is_some());
        assert!(lookup.find(&second, b"value-0000").is_none());
    }

    /// A dictionary built in memory does not know its own order, and the caller has to be left on
    /// the path it would have taken, which is the peel.
    #[test]
    fn a_dictionary_with_no_order_is_declined() {
        let values = Arc::new(letters(&["apple", "pear"]));
        let column = Vector::stable_dictionary(vec![0, 1], values).expect("codes");
        assert!(Lookup::default().find(&column, b"apple").is_none());
    }
}
