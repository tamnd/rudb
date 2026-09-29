//! Which runs of three bytes a chunk of a string column holds, so a `LIKE` can skip the chunk.
//!
//! A range says nothing about `URL LIKE '%google%'`, because a chunk whose smallest and largest URL
//! are `http://a` and `http://z` can hold any word at all in between. What can rule the chunk out is
//! the word itself: a row that holds `google` holds `goo`, `oog`, `ogl` and `gle`, so a chunk
//! where one of those four is missing from every row cannot hold a match. On the ClickBench `hits`
//! sample at a million rows, `URL LIKE '%google%'` matches in 33 of 489 chunks and this keeps 93 of
//! them, and `Title LIKE '%Google%'` matches in 215 and this keeps exactly those 215.
//!
//! # One sided
//!
//! [`Grams::lacks`] says no row of the chunk holds the text, or it says nothing, the same as a
//! [`crate::Sieve`] does. Every run of three bytes of every row goes in, so a run that is missing
//! is missing from all of them. Two runs can land on one bit, which keeps a chunk that could have
//! gone and never drops one that could not. A chunk with a row that cannot be read as bytes, which
//! is a compressed one, has no grams at all rather than grams for the rows that could be read.
//!
//! # What it costs
//!
//! Four kilobytes a chunk, which is two bytes a row, and one pass over the column's bytes the first
//! time a `LIKE` asks about it. Nothing is built at load: a column nobody runs a `LIKE` over never
//! pays for it. On the `hits` sample the busiest chunk of `URL` sets about a fifth of the bits, so
//! a word of six letters is kept by a chunk that lacks it about one time in six hundred.

use rudb_vector::Vector;

/// How many bits a chunk's grams are, as a power of two.
const BITS: u32 = 15;

/// The same, in words.
const WORDS: usize = 1 << (BITS - 6);

/// The runs of three bytes one chunk of one string column holds, folded onto a bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grams {
    bits: Box<[u64]>,
}

impl Grams {
    /// The grams of every row of `vector`, or `None` when one of its rows cannot be read as bytes.
    ///
    /// A null row holds nothing and adds nothing. A row that repeats the one before it is skipped,
    /// which is most of what a column of URLs does inside a chunk and costs one comparison.
    #[must_use]
    pub fn of(vector: &Vector) -> Option<Self> {
        let mut bits = vec![0_u64; WORDS].into_boxed_slice();
        let mut last: Option<&[u8]> = None;
        for row in 0..vector.len() {
            let Some(bytes) = vector.bytes_at(row) else {
                if vector.is_null_at(row) {
                    continue;
                }
                return None;
            };
            if last == Some(bytes) {
                continue;
            }
            last = Some(bytes);
            for gram in bytes.windows(3) {
                let at = slot(gram);
                bits[at >> 6] |= 1 << (at & 63);
            }
        }
        Some(Self { bits })
    }

    /// Whether no row of the chunk can hold `text`.
    ///
    /// Text shorter than three bytes has no gram to be missing and so is never lacked.
    #[must_use]
    pub fn lacks(&self, text: &[u8]) -> bool {
        text.windows(3).any(|gram| {
            let at = slot(gram);
            self.bits[at >> 6] & (1 << (at & 63)) == 0
        })
    }

    /// The bytes this holds, for a table adding up what it owns.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.bits.len() * size_of::<u64>()
    }
}

/// The bit one run of three bytes lands on: the three bytes as a number, multiplied by a large odd
/// constant, and the top bits of that.
fn slot(gram: &[u8]) -> usize {
    let packed = u64::from(gram[0]) | u64::from(gram[1]) << 8 | u64::from(gram[2]) << 16;
    (packed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - BITS)) as usize
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};

    use super::*;

    fn words(values: &[Option<&str>]) -> Vector {
        let values: Vec<Value> = values
            .iter()
            .map(|value| value.map_or(Value::Null, |text| Value::Varchar(text.to_string())))
            .collect();
        Vector::from_values(LogicalType::Varchar, &values).expect("a string column")
    }

    #[test]
    fn a_chunk_without_the_word_lacks_it_and_one_with_it_does_not() {
        let grams = Grams::of(&words(&[Some("http://example.com/"), None, Some("http://go.com/")]))
            .expect("every row reads");
        assert!(grams.lacks(b"google"));
        assert!(!grams.lacks(b"example"));
        assert!(!grams.lacks(b"go.c"));
        assert!(!grams.lacks(b"go"), "two bytes have no gram to miss");
    }

    #[test]
    fn a_word_split_across_two_rows_is_not_lacked() {
        // The grams are per chunk, so `goo` in one row and `gle` in another keep the chunk. That is
        // a chunk read for nothing and never a match missed.
        let grams = Grams::of(&words(&[Some("goog"), Some("ogle")])).expect("every row reads");
        assert!(!grams.lacks(b"google"));
    }

    #[test]
    fn a_dictionary_is_read_through_its_codes() {
        let values = words(&[Some("apple"), Some("google")]);
        let grams = Grams::of(&Vector::dictionary(vec![0, 0, 0], values).expect("three rows"))
            .expect("every row reads");
        assert!(grams.lacks(b"google"), "no row points at google");
        assert!(!grams.lacks(b"apple"));
    }
}
