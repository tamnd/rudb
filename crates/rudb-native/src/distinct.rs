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

/// The most slots one column's sets may hold between them, which is 256 MiB of keys.
///
/// At the load factor below that is a little over twenty nine million distinct values, which covers
/// `UserID` at the full hundred million row `hits` table, seventeen and a half million, and gives up
/// on `WatchID`, which is near unique there and whose count the row path already gets right.
const MAX_SLOTS: usize = 1 << 25;

/// How many distinct values a column may have and still be counted, the seven eighths of
/// [`MAX_SLOTS`] one table of that size would have held before it had to grow past the cap.
const MAX_DISTINCT: usize = MAX_SLOTS / 8 * 7;

/// How many sets a column's values are spread over, by the top bits of their hash.
///
/// One set of twenty nine million keys is 256 MiB, and a value landing anywhere in it is a miss in
/// every cache and in the page table as well, which is what the first pass over `WatchID` spent a
/// third of its time on. A value goes to a buffer for its set instead, and a set is only touched
/// when its buffer is full, so each touch is a run of inserts into one part of a 256th the size.
const SETS: usize = 1 << SET_BITS;
const SET_BITS: u32 = 8;

/// How many values wait for their set, which is 128 KiB of buffers across [`SETS`] and fits in the
/// second level cache beside the set being filled.
const BUFFERED: usize = 64;

/// One set's first size, small enough that a column of flags does not pay for a large one.
const FIRST_SLOTS: usize = 1 << 4;

/// Values stay under seven eighths of the slots.
///
/// Linear probing degrades as the table fills, but the keys here are hashed by a multiply that
/// spreads consecutive integers, which are the common case, evenly, and seven eighths keeps the
/// largest table's worth of keys inside `MAX_SLOTS` rather than needing twice the memory.
fn full(len: usize, slots: usize) -> bool {
    len * 8 >= slots * 7
}

/// Fibonacci hashing, the value times the golden ratio in sixty four bits.
///
/// The multiplier is odd, so this is a bijection of the sixty four bit values and the sets hold the
/// hashes rather than the values: two hashes are equal exactly when the values are, and zero is
/// still only ever the hash of zero.
fn hash(value: u64) -> u64 {
    value.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// An exact set of one column's distinct non-null values, or the record that there were too many.
#[derive(Debug)]
pub(crate) struct ExactDistinct {
    /// One open addressed set per top eight bits of the hash. Zero marks an empty slot, so a zero
    /// value is held in [`Self::zero`] instead.
    sets: Vec<Vec<u64>>,
    /// How many hashes each set holds.
    held: Vec<usize>,
    /// [`BUFFERED`] hashes per set waiting to go in, and how many of each are there.
    buffered: Vec<u64>,
    waiting: Vec<u8>,
    zero: bool,
    len: usize,
    /// Set when the cap was passed. The sets are released at that point rather than at the end.
    gave_up: bool,
}

impl ExactDistinct {
    pub(crate) fn new() -> Self {
        Self {
            sets: vec![vec![0; FIRST_SLOTS]; SETS],
            held: vec![0; SETS],
            buffered: vec![0; SETS * BUFFERED],
            waiting: vec![0; SETS],
            zero: false,
            len: 0,
            gave_up: false,
        }
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
        let hash = hash(value);
        let set = (hash >> (64 - SET_BITS)) as usize;
        let waiting = usize::from(self.waiting[set]);
        self.buffered[set * BUFFERED + waiting] = hash;
        self.waiting[set] = (waiting + 1) as u8;
        if waiting + 1 == BUFFERED {
            self.drain(set);
        }
    }

    /// The count, or `None` for a column that went past the cap.
    pub(crate) fn count(&mut self) -> Option<u64> {
        for set in 0..SETS {
            if self.gave_up {
                break;
            }
            self.drain(set);
        }
        (!self.gave_up).then(|| self.len as u64 + u64::from(self.zero))
    }

    /// Moves one set's waiting hashes into it.
    ///
    /// Whether a column gives up depends only on how many distinct values it has, never on the
    /// order they were drained in, because the count only goes up and the cap is on the total.
    fn drain(&mut self, set: usize) {
        let waiting = usize::from(std::mem::take(&mut self.waiting[set]));
        let from = set * BUFFERED;
        touch(&self.sets[set], &self.buffered[from..from + waiting]);
        for at in from..from + waiting {
            let hash = self.buffered[at];
            if !place(&mut self.sets[set], hash) {
                continue;
            }
            self.held[set] += 1;
            self.len += 1;
            if full(self.held[set], self.sets[set].len()) {
                let wanted = self.sets[set].len() * 2;
                let old = std::mem::replace(&mut self.sets[set], vec![0; wanted]);
                for hash in old.into_iter().filter(|&hash| hash != 0) {
                    place(&mut self.sets[set], hash);
                }
            }
        }
        if self.len >= MAX_DISTINCT {
            self.gave_up = true;
            self.sets = Vec::new();
            self.buffered = Vec::new();
        }
    }
}

/// Reads the slot each of `hashes` starts at, before any of them is placed.
///
/// A set of a column that is near unique is half a megabyte, so the slot a hash starts at is a
/// cache miss nearly every time, and [`place`] took them one after another, since each insert
/// waits on its own load before the next one begins. These loads do not depend on each other, so
/// the processor has all of them in flight at once, and the inserts after them find their lines in
/// cache. It was about fifty nanoseconds an insert on a column of ten million distinct values.
fn touch(slots: &[u64], hashes: &[u64]) {
    let mut seen = 0_u64;
    for &hash in hashes {
        seen ^= slots[home(slots, hash)];
    }
    std::hint::black_box(seen);
}

/// The slot a search for `hash` starts at.
fn home(slots: &[u64], hash: u64) -> usize {
    ((hash << SET_BITS) >> (64 - slots.len().trailing_zeros())) as usize
}

/// Puts a nonzero hash in its slot and says whether it was new.
///
/// The slot comes from the bits under the ones that chose the set, which every hash in the set
/// shares.
fn place(slots: &mut [u64], hash: u64) -> bool {
    let mask = slots.len() - 1;
    let mut at = home(slots, hash);
    loop {
        match slots[at] {
            0 => {
                slots[at] = hash;
                return true;
            }
            held if held == hash => return false,
            _ => at = (at + 1) & mask,
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
        // One short of the cap is counted, and the value that reaches it is not, whichever order
        // the buffers happened to drain in.
        let mut set = ExactDistinct::new();
        for value in 1..MAX_DISTINCT as u64 {
            set.insert(value);
        }
        set.insert(0);
        assert_eq!(set.count(), Some(MAX_DISTINCT as u64), "gave up before the cap");
        set.insert(MAX_DISTINCT as u64);
        assert_eq!(set.count(), None, "counted past the cap");
        assert!(set.sets.is_empty(), "a column that gave up still holds its table");
    }
}
