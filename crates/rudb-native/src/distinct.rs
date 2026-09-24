//! An exact distinct count for an integer column, and the rows holding each value, taken by the
//! writer on a pass it already makes.
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

use std::collections::HashMap;

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

/// Whether a column the sketch puts at `estimate` distinct values is so far past the cap that
/// building its set would only fill 256 MiB and give up.
///
/// Twice the cap, where the sketch's error of a percent or two cannot reach, so a column this
/// turns away is one the set would have turned away too. The sketch is a function of the column's
/// values alone, so the answer, and the file, does not depend on the machine or the order the
/// close ran in.
pub(crate) fn beyond(estimate: f64) -> bool {
    estimate > (2 * MAX_DISTINCT) as f64
}

/// About the most memory a set holds while it counts a column of `estimate` distinct values.
///
/// Each of the [`SETS`] sets takes its share of the values, an eighth over to cover a share that
/// came out uneven and the sketch's error, at the power of two its growth rule reaches. Past the
/// cap it is the size the sets reach as the last of them grow and the column gives up. This is
/// what the close charges a column before it starts, so it errs large.
pub(crate) fn bytes_for(estimate: f64) -> usize {
    let values = estimate.clamp(0.0, MAX_DISTINCT as f64) as usize;
    let share = (values / SETS).saturating_mul(9) / 8 + 1;
    let mut slots = FIRST_SLOTS;
    while full(share, slots) {
        slots *= 2;
    }
    (slots + BUFFERED) * SETS * size_of::<u64>()
}

/// The low bits of a slot, which hold how many rows have the slot's value.
///
/// A slot is one `u64`. The top eight bits of a hash choose its set, so every hash in a set has the
/// same top eight bits, and shifted up by them the other fifty six fill the slot and leave eight at
/// the bottom free. Those hold the count, so a set that counts is no larger than one that only told
/// values apart. A count that reaches [`SPILLED`] moves to a map beside the sets, and on a column of
/// `n` rows at most `n / 255` values ever get there.
const COUNT_BITS: u32 = 8;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;

/// The count a slot reads when its real count is in [`ExactCounts::spilled`].
const SPILLED: u64 = COUNT_MASK;

/// The inverse of the multiplier in [`hash`], so a hash turns back into the value it came from.
const UNHASH: u64 = inverse(0x9E37_79B9_7F4A_7C15);

/// The inverse of an odd number modulo two to the sixty four, by Newton's iteration. An odd number
/// is its own inverse in the low three bits, and each step doubles the bits that are right.
const fn inverse(odd: u64) -> u64 {
    let mut inverse = odd;
    let mut step = 0;
    while step < 5 {
        inverse = inverse.wrapping_mul(2_u64.wrapping_sub(odd.wrapping_mul(inverse)));
        step += 1;
    }
    inverse
}

/// Every distinct non-null value of one column and how many rows hold it, or the record that there
/// were too many values to keep.
///
/// Counting the rows beside each value is what lets the close take a column's frequencies from the
/// same pass that counts its distinct values. Before this, a column past the thirty two thousand
/// values of the candidate table went through a Misra-Gries table and this set side by side, and
/// then through a second read of every page to recount the candidates. On the `hits_0` load that
/// was most of the close for the dozen columns with hundreds of thousands of values.
#[derive(Debug)]
pub(crate) struct ExactCounts {
    /// One open addressed set per top eight bits of the hash, each slot the rest of the hash above
    /// its count. Zero marks an empty slot, which no held value is, since its count is at least one.
    sets: Vec<Vec<u64>>,
    /// How many values each set holds.
    held: Vec<usize>,
    /// [`BUFFERED`] hashes per set waiting to go in, and how many of each are there.
    buffered: Vec<u64>,
    waiting: Vec<u8>,
    /// The counts too large for a slot, by hash.
    spilled: HashMap<u64, u64, crate::Spread>,
    len: usize,
    /// Set when the cap was passed. The sets are released at that point rather than at the end.
    gave_up: bool,
}

impl ExactCounts {
    pub(crate) fn new() -> Self {
        Self {
            sets: vec![vec![0; FIRST_SLOTS]; SETS],
            held: vec![0; SETS],
            buffered: vec![0; SETS * BUFFERED],
            waiting: vec![0; SETS],
            spilled: HashMap::default(),
            len: 0,
            gave_up: false,
        }
    }

    /// Adds `times` rows of one value's bits.
    ///
    /// A single row waits in its set's buffer. A run of equal rows goes straight in, since it is
    /// one probe for all of them and there is no order among the adds to keep.
    pub(crate) fn insert(&mut self, value: u64, times: u32) {
        if self.gave_up || times == 0 {
            return;
        }
        let hash = hash(value);
        let set = (hash >> (64 - SET_BITS)) as usize;
        if times > 1 {
            self.add(set, hash, u64::from(times));
            self.check_cap();
            return;
        }
        let waiting = usize::from(self.waiting[set]);
        self.buffered[set * BUFFERED + waiting] = hash;
        self.waiting[set] = (waiting + 1) as u8;
        if waiting + 1 == BUFFERED {
            self.drain(set);
        }
    }

    /// The count of distinct values, or `None` for a column that went past the cap.
    pub(crate) fn count(&mut self) -> Option<u64> {
        self.drain_all();
        (!self.gave_up).then_some(self.len as u64)
    }

    /// Hands every value and the rows holding it to `visit`, in no particular order, or does nothing
    /// and says `false` for a column that went past the cap.
    pub(crate) fn visit(&mut self, mut visit: impl FnMut(u64, u64)) -> bool {
        self.drain_all();
        if self.gave_up {
            return false;
        }
        for (set, slots) in self.sets.iter().enumerate() {
            for &slot in slots.iter().filter(|&&slot| slot != 0) {
                let hash = ((set as u64) << (64 - SET_BITS)) | ((slot & !COUNT_MASK) >> SET_BITS);
                let count = match slot & COUNT_MASK {
                    SPILLED => self.spilled.get(&hash).copied().unwrap_or(SPILLED),
                    count => count,
                };
                visit(hash.wrapping_mul(UNHASH), count);
            }
        }
        true
    }

    fn drain_all(&mut self) {
        for set in 0..SETS {
            if self.gave_up {
                break;
            }
            self.drain(set);
        }
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
            self.add(set, hash, 1);
        }
        self.check_cap();
    }

    /// Adds `times` rows of the value with this hash to its set, growing the set when it fills.
    fn add(&mut self, set: usize, hash: u64, times: u64) {
        let key = hash << SET_BITS;
        let slots = &mut self.sets[set];
        let mask = slots.len() - 1;
        let mut at = home(slots, key);
        loop {
            let slot = slots[at];
            if slot == 0 {
                slots[at] = if times >= SPILLED {
                    self.spilled.insert(hash, times);
                    key | SPILLED
                } else {
                    key | times
                };
                self.held[set] += 1;
                self.len += 1;
                if full(self.held[set], slots.len()) {
                    let wanted = slots.len() * 2;
                    let old = std::mem::replace(slots, vec![0; wanted]);
                    for slot in old.into_iter().filter(|&slot| slot != 0) {
                        place(slots, slot);
                    }
                }
                return;
            }
            if slot & !COUNT_MASK == key {
                let count = slot & COUNT_MASK;
                if count == SPILLED {
                    *self.spilled.entry(hash).or_insert(SPILLED) += times;
                } else if count + times >= SPILLED {
                    self.spilled.insert(hash, count + times);
                    slots[at] = key | SPILLED;
                } else {
                    slots[at] = slot + times;
                }
                return;
            }
            at = (at + 1) & mask;
        }
    }

    fn check_cap(&mut self) {
        if self.len > MAX_DISTINCT {
            self.gave_up = true;
            self.sets = Vec::new();
            self.buffered = Vec::new();
            self.spilled = HashMap::default();
        }
    }
}

/// Reads the slot each of `hashes` starts at, before any of them is placed.
///
/// A set of a column that is near unique is half a megabyte, so the slot a hash starts at is a
/// cache miss nearly every time, and [`ExactCounts::add`] took them one after another, since each
/// insert waits on its own load before the next one begins. These loads do not depend on each
/// other, so the processor has all of them in flight at once, and the inserts after them find their
/// lines in cache. On a column of ten million distinct values that took the count from about half a
/// second to about 350 milliseconds.
fn touch(slots: &[u64], hashes: &[u64]) {
    let mut seen = 0_u64;
    for &hash in hashes {
        seen ^= slots[home(slots, hash << SET_BITS)];
    }
    std::hint::black_box(seen);
}

/// The slot a search for a slot's key, the hash shifted past the bits that chose its set, starts at.
fn home(slots: &[u64], key: u64) -> usize {
    (key >> (64 - slots.len().trailing_zeros())) as usize
}

/// Puts a held slot, key and count, in the first empty place from its home, for a set that grew.
fn place(slots: &mut [u64], slot: u64) {
    let mask = slots.len() - 1;
    let mut at = home(slots, slot & !COUNT_MASK);
    while slots[at] != 0 {
        at = (at + 1) & mask;
    }
    slots[at] = slot;
}

/// The same counts for a column whose values all sit in a short range, one `u32` per value in it.
///
/// A column's statistics already hold its lowest and highest value, and on most integer columns the
/// range between them is not much wider than the number of values: dates, line numbers, and keys
/// into a smaller table. Counting those by `value - low` into a flat array is a load and a store a
/// row, where the set above is a hash, a buffer and then a probe. On a `lineitem` load the set was
/// about 6% of the load's cycles.
///
/// The offset is taken on the sixty four bits the close keys values by, so it is the same sum for a
/// signed column and an unsigned one. A value outside the range, which a column whose ends were
/// read correctly does not have, marks the count as unusable and the caller counts again with
/// [`ExactCounts`] rather than trusting it.
#[derive(Debug)]
pub(crate) struct DenseCounts {
    low: u64,
    counts: Vec<u32>,
    outside: bool,
}

impl DenseCounts {
    /// Counts for `len` values starting at the one whose bits are `low`.
    pub(crate) fn new(low: u64, len: usize) -> Self {
        Self { low, counts: vec![0; len], outside: false }
    }

    /// Adds `times` rows of one value's bits.
    ///
    /// The caller only builds one of these for a table of fewer than `u32::MAX` rows, so a count
    /// cannot overflow.
    pub(crate) fn insert(&mut self, value: u64, times: u32) {
        let at = value.wrapping_sub(self.low);
        match usize::try_from(at).ok().and_then(|at| self.counts.get_mut(at)) {
            Some(count) => *count += times,
            None => self.outside = true,
        }
    }

    /// The count of distinct values, `Some(None)` for a column past the cap [`ExactCounts`] has, and
    /// `None` when a value fell outside the range and the counts cannot be used.
    pub(crate) fn count(&self) -> Option<Option<u64>> {
        if self.outside {
            return None;
        }
        let distinct = self.counts.iter().filter(|&&count| count != 0).count();
        Some((distinct <= MAX_DISTINCT).then_some(distinct as u64))
    }

    /// Hands every value and the rows holding it to `visit`, in the order of their bits above
    /// `low`.
    pub(crate) fn visit(&self, mut visit: impl FnMut(u64, u64)) {
        for (at, &count) in self.counts.iter().enumerate().filter(|(_, count)| **count != 0) {
            visit(self.low.wrapping_add(at as u64), u64::from(count));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(set: &mut ExactCounts) -> HashMap<u64, u64> {
        let mut counts = HashMap::new();
        assert!(set.visit(|value, count| assert!(counts.insert(value, count).is_none())));
        counts
    }

    #[test]
    fn dense_counts_agree_with_the_set_and_notice_a_value_outside() {
        let low = (-40_i64) as u64;
        let mut dense = DenseCounts::new(low, 100);
        let mut set = ExactCounts::new();
        for at in 0..10_000_i64 {
            let value = (at * 37 % 97 - 40) as u64;
            let times = u32::try_from(at % 3 + 1).expect("small");
            dense.insert(value, times);
            set.insert(value, times);
        }
        assert_eq!(dense.count(), Some(set.count()));
        let mut counts = HashMap::new();
        dense.visit(|value, count| assert!(counts.insert(value, count).is_none()));
        assert_eq!(counts, counted(&mut set));
        dense.insert(60, 1);
        assert_eq!(dense.count(), None);
    }

    #[test]
    fn a_hash_turns_back_into_its_value() {
        for value in [0, 1, 2, u64::MAX, 1 << 63, 0x0123_4567_89AB_CDEF] {
            assert_eq!(hash(value).wrapping_mul(UNHASH), value);
        }
    }

    #[test]
    fn counts_each_value_and_its_rows_across_growth_and_counts_zero() {
        let mut set = ExactCounts::new();
        let mut oracle = HashMap::new();
        for round in 0..3 {
            for value in 0..50_000_u64 {
                // Spread across the whole width, and negative numbers as their two's complement bits,
                // which is how a signed column arrives here. The two runs share zero and a few hundred
                // other values, which is what the oracle is for.
                for bits in [value.wrapping_mul(0x0123_4567_89AB_CDEF), (-(value as i64)) as u64] {
                    set.insert(bits, 1);
                    *oracle.entry(bits).or_insert(0) += 1;
                }
            }
            assert_eq!(set.count(), Some(oracle.len() as u64), "round {round} counted wrong");
            assert_eq!(counted(&mut set), oracle, "round {round} counted the rows wrong");
        }
    }

    #[test]
    fn a_count_past_a_slot_moves_beside_the_set_and_keeps_counting() {
        let mut set = ExactCounts::new();
        for _ in 0..300 {
            set.insert(7, 1);
        }
        set.insert(9, 254);
        set.insert(9, 1);
        set.insert(11, 1_000);
        set.insert(11, 3);
        for _ in 0..254 {
            set.insert(13, 1);
        }
        let counts = counted(&mut set);
        assert_eq!(counts, [(7, 300), (9, 255), (11, 1_003), (13, 254)].into_iter().collect());
        assert_eq!(set.count(), Some(4));
    }

    #[test]
    fn the_estimate_covers_what_a_set_holds_and_not_much_more() {
        for distinct in [0_usize, 1, 1_000, 40_000, 300_000, 1_000_000] {
            let mut set = ExactCounts::new();
            for value in 1..=distinct as u64 {
                set.insert(value.wrapping_mul(0x0123_4567_89AB_CDEF), 1);
            }
            assert_eq!(set.count(), Some(distinct as u64));
            let held = (set.sets.iter().map(Vec::len).sum::<usize>() + set.buffered.len()) * 8;
            let estimate = bytes_for(distinct as f64);
            assert!(held <= estimate, "{distinct} values held {held} bytes over {estimate}");
            assert!(
                estimate <= 2 * held,
                "{distinct} values held {held} bytes, far under {estimate}"
            );
        }
        assert!(
            bytes_for(1e12) <= (512 << 20) + (1 << 20),
            "past the cap is not the most a set holds"
        );
        assert!(beyond(3.0 * MAX_DISTINCT as f64));
        assert!(!beyond(MAX_DISTINCT as f64));
    }

    #[test]
    fn a_column_past_the_cap_records_nothing() {
        // The cap is counted, and the value past it is not, whichever order the buffers happened to
        // drain in.
        let mut set = ExactCounts::new();
        for value in 0..MAX_DISTINCT as u64 {
            set.insert(value, 1);
        }
        assert_eq!(set.count(), Some(MAX_DISTINCT as u64), "gave up before the cap");
        set.insert(MAX_DISTINCT as u64, 2);
        assert_eq!(set.count(), None, "counted past the cap");
        assert!(set.sets.is_empty(), "a column that gave up still holds its table");
        assert!(!set.visit(|_, _| panic!("a column that gave up handed over a value")));
    }
}
