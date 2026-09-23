//! An exact distinct count for an integer column, taken by the writer on a pass it already makes.
//!
//! A string column's distinct count is free, because the global dictionary hands out one code per
//! value and the writer counts the codes. An integer column has no dictionary, so until this the
//! directory said nothing about it and `COUNT(DISTINCT UserID)` built a hash set over every row at
//! query time while `COUNT(DISTINCT SearchPhrase)` read one number. Document 31 measured the pair on
//! the same table: 165 times ahead of DuckDB for the string and level for the integer.
//!
//! The numeric frequency pass already decodes every value of every integer column once. Adding each
//! one to a set on the way past is the whole cost, and the set is the flattest one there is: the
//! value's sixty four bits in a power of two array, found by linear probing. Every integer type the
//! format stores widens into `i128` without loss and narrows back into the bits it came from, so
//! within one column the truncation to `u64` is injective and two values collide only if they are
//! the same value.
//!
//! The set is capped, and a column past the cap records nothing. That is the ordinary answer for a
//! column nobody counted and the reader already handles it by counting the rows. The cap is per
//! column rather than shared across the frequency workers, so the file a load writes does not depend
//! on which worker reached which column first.

/// The most slots one column's set may hold, which is 256 MiB of keys.
///
/// At the load factor below that is a little over twenty nine million distinct values, which covers
/// `UserID` at the full hundred million row `hits` table, seventeen and a half million, and gives up
/// on `WatchID`, which is near unique there and whose count the row path already gets right.
const MAX_SLOTS: usize = 1 << 25;

/// The first table's size, small enough that a column of flags does not pay for a large one.
const FIRST_SLOTS: usize = 1 << 10;

/// Values stay under seven eighths of the slots.
///
/// Linear probing degrades as the table fills, but the keys here are hashed by a multiply that
/// spreads consecutive integers, which are the common case, evenly, and seven eighths keeps the
/// largest table's worth of keys inside `MAX_SLOTS` rather than needing twice the memory.
fn full(len: usize, slots: usize) -> bool {
    len * 8 >= slots * 7
}

/// Fibonacci hashing, the top bits of the value times the golden ratio in sixty four bits.
fn slot(value: u64, shift: u32) -> usize {
    (value.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> shift) as usize
}

/// An exact set of one column's distinct non-null values, or the record that there were too many.
#[derive(Debug)]
pub(crate) struct ExactDistinct {
    /// Zero marks an empty slot, so a zero value is held in [`Self::zero`] instead.
    slots: Vec<u64>,
    zero: bool,
    len: usize,
    /// Set when the cap was passed. The slots are released at that point rather than at the end.
    gave_up: bool,
}

impl ExactDistinct {
    pub(crate) fn new() -> Self {
        Self { slots: vec![0; FIRST_SLOTS], zero: false, len: 0, gave_up: false }
    }

    /// Adds one value's bits.
    pub(crate) fn insert(&mut self, value: u64) {
        if self.gave_up {
            return;
        }
        if value == 0 {
            self.zero = true;
            return;
        }
        if !self.place(value) {
            return;
        }
        self.len += 1;
        if full(self.len, self.slots.len()) {
            self.grow();
        }
    }

    /// The count, or `None` for a column that went past the cap.
    pub(crate) fn count(&self) -> Option<u64> {
        (!self.gave_up).then(|| self.len as u64 + u64::from(self.zero))
    }

    /// Puts a nonzero value in its slot and says whether it was new.
    fn place(&mut self, value: u64) -> bool {
        let mask = self.slots.len() - 1;
        let mut at = slot(value, 64 - self.slots.len().trailing_zeros());
        loop {
            match self.slots[at] {
                0 => {
                    self.slots[at] = value;
                    return true;
                }
                held if held == value => return false,
                _ => at = (at + 1) & mask,
            }
        }
    }

    fn grow(&mut self) {
        let wanted = self.slots.len() * 2;
        if wanted > MAX_SLOTS {
            self.gave_up = true;
            self.slots = Vec::new();
            return;
        }
        let old = std::mem::replace(&mut self.slots, vec![0; wanted]);
        for value in old.into_iter().filter(|&value| value != 0) {
            self.place(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_value_once_across_growth_and_counts_zero() {
        let mut set = ExactDistinct::new();
        let mut oracle = std::collections::HashSet::new();
        for round in 0..3 {
            for value in 0..50_000_u64 {
                // Spread across the whole width, and negative numbers as their two's complement bits,
                // which is how a signed column arrives here. The two runs share zero and a few hundred
                // other values, which is what the oracle is for.
                for bits in [value.wrapping_mul(0x0123_4567_89AB_CDEF), (-(value as i64)) as u64] {
                    set.insert(bits);
                    oracle.insert(bits);
                }
            }
            assert_eq!(set.count(), Some(oracle.len() as u64), "round {round} counted wrong");
        }
    }

    #[test]
    fn a_column_past_the_cap_records_nothing() {
        let mut set = ExactDistinct::new();
        let mut value = 1_u64;
        while set.count().is_some() {
            set.insert(value);
            value += 1;
        }
        assert!(value as usize > MAX_SLOTS / 8 * 7, "gave up before the cap");
        assert!(set.slots.is_empty(), "a column that gave up still holds its table");
    }
}
