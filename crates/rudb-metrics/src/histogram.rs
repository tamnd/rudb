//! A latency histogram with a fixed relative error, in the layout HdrHistogram uses.
//!
//! `notes/Spec/2140/engine-v4/16-measurement.md` section 16.2 asks for commit latency, durability
//! wait, lock waits, sync latency and bytes per sync as histograms, and the benchmark drivers in
//! `rudb-bench` need the same thing for every operation they time. A mean hides the tail and a
//! sorted vector of samples is a gigabyte after a long run, so the answer everyone lands on is a
//! histogram whose buckets get wider as the values get larger, which keeps the error a fixed
//! fraction of the value rather than a fixed number of nanoseconds.
//!
//! # The layout
//!
//! Two significant digits means any value up to 200 is kept exactly and any larger value is kept to
//! within one part in a hundred. That needs 256 slots in the first bucket, a power of two because
//! the index is computed with shifts. Every bucket after the first covers twice the range of the one
//! before with the same 128 slots in its upper half, so its resolution halves as its range doubles
//! and the relative error stays where it was. The lower half of every bucket but the first would
//! repeat values the bucket below already holds, and it is not stored.
//!
//! The range is 1 ns to 100 s, which is 30 buckets and 3,968 slots, 31 KiB of `u64`. That is a
//! little more than the 24 KiB the spec estimated, and it is per worker, which is where the spec
//! wants it: a worker records into its own histogram with plain adds, and a reader merges them,
//! and merging two of these is adding two arrays.
//!
//! A value past the top is recorded at the top and counted in [`Histogram::clamped`], rather than
//! dropped, so a count of operations is never short. A stall that long is the thing somebody wants
//! to see, and the report says how many samples it clamped.
//!
//! It is written here rather than taken from the `hdrhistogram` crate because neither this
//! workspace nor `rudb-bench` takes an outside dependency for something this size, and because the
//! engine and the drivers have to agree on the layout to the bit: a histogram a driver records and a
//! histogram the engine reports are merged and compared, which only works when both came from here.

/// Values up to this are kept exactly. Two significant digits is two times ten to the second.
const EXACT: u64 = 200;

/// Slots in one bucket, the smallest power of two at or above [`EXACT`].
const SLOTS: u64 = EXACT.next_power_of_two();

/// The power of two [`SLOTS`] is, minus one, which is the size of the upper half of a bucket.
const HALF_MAGNITUDE: u32 = SLOTS.trailing_zeros() - 1;

/// Slots in the upper half of a bucket, which is every slot a bucket past the first stores.
const HALF: u64 = 1 << HALF_MAGNITUDE;

/// The largest value recorded as itself, 100 s in nanoseconds.
pub const HIGHEST: u64 = 100_000_000_000;

/// How many buckets it takes to reach [`HIGHEST`].
const BUCKETS: u32 = {
    let mut buckets = 1;
    let mut reach = SLOTS;
    while reach <= HIGHEST {
        reach <<= 1;
        buckets += 1;
    }
    buckets
};

/// The number of stored slots: the whole of the first bucket and the upper half of the rest.
const LEN: usize = ((BUCKETS + 1) as u64 * HALF) as usize;

/// A histogram of `u64` values, 1 to 100 billion, two significant digits.
///
/// Recording is two shifts, a count of leading zeros and an add. Nothing in it is atomic, because
/// the one that is written on every operation belongs to one worker and the one that is read is a
/// merge of those, per section 16.2.
#[derive(Clone, PartialEq, Eq)]
pub struct Histogram {
    counts: Box<[u64]>,
    total: u64,
    clamped: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histogram")
            .field("count", &self.total)
            .field("min", &self.min())
            .field("p50", &self.quantile(0.5))
            .field("p99", &self.quantile(0.99))
            .field("max", &self.max)
            .finish_non_exhaustive()
    }
}

/// The stored slot a value goes in.
fn slot(value: u64) -> usize {
    // The bucket is how far the value's top bit sits above the top bit of the first bucket, and
    // or-ing the mask in first puts every value that fits in the first bucket at bucket zero.
    let bucket = u64::from(63 - (value | (SLOTS - 1)).leading_zeros() - HALF_MAGNITUDE);
    let within = value >> bucket;
    // Bucket zero uses all of its slots from zero, and every later bucket starts its stored slots
    // at the middle, so the index is the bucket's base plus how far past the middle the value is.
    ((bucket * HALF) + within) as usize
}

/// The lowest value that lands in stored slot `index`.
fn lowest(index: usize) -> u64 {
    let index = index as u64;
    let bucket = (index / HALF).saturating_sub(1);
    let within = index - bucket * HALF;
    within << bucket
}

/// The highest value that lands in stored slot `index`.
fn highest(index: usize) -> u64 {
    let index = index as u64;
    let bucket = (index / HALF).saturating_sub(1);
    lowest(index as usize) + (1 << bucket) - 1
}

impl Histogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: vec![0; LEN].into_boxed_slice(),
            total: 0,
            clamped: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    /// Record one value.
    pub fn record(&mut self, value: u64) {
        self.record_n(value, 1);
    }

    /// Record `n` copies of one value, which is how a driver corrects for coordinated omission: an
    /// operation that stalled for ten intervals is recorded as the ten operations that would have
    /// waited behind it.
    pub fn record_n(&mut self, value: u64, n: u64) {
        if n == 0 {
            return;
        }
        let kept = if value > HIGHEST {
            self.clamped += n;
            HIGHEST
        } else {
            value
        };
        self.counts[slot(kept)] += n;
        self.total += n;
        self.sum += u128::from(kept) * u128::from(n);
        self.min = self.min.min(kept);
        self.max = self.max.max(kept);
    }

    /// Add everything `other` recorded to this one.
    pub fn merge(&mut self, other: &Histogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            *mine += theirs;
        }
        self.total += other.total;
        self.clamped += other.clamped;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    /// Forget everything, keeping the allocation.
    pub fn reset(&mut self) {
        self.counts.fill(0);
        self.total = 0;
        self.clamped = 0;
        self.min = u64::MAX;
        self.max = 0;
        self.sum = 0;
    }

    /// How many values were recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// How many of them were past [`HIGHEST`] and were recorded as it.
    #[must_use]
    pub fn clamped(&self) -> u64 {
        self.clamped
    }

    /// The smallest value recorded, exactly, or zero when nothing was.
    #[must_use]
    pub fn min(&self) -> u64 {
        if self.total == 0 { 0 } else { self.min }
    }

    /// The largest value recorded, exactly, or zero when nothing was.
    #[must_use]
    pub fn max(&self) -> u64 {
        self.max
    }

    /// The mean of the values recorded, exactly, or zero when nothing was.
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.sum as f64 / self.total as f64 }
    }

    /// The value at quantile `q`, between 0 and 1.
    ///
    /// The answer is the highest value in the slot where the running count first reaches `q` of the
    /// total, capped at the largest value actually recorded. That is what HdrHistogram answers and
    /// it errs upward, which is the right way for a latency to err: a p99 that is reported a little
    /// high is a bound, and one reported a little low is a claim nobody measured. Zero when nothing
    /// was recorded.
    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        if q <= 0.0 {
            return self.min;
        }
        let q = q.min(1.0);
        let wanted = ((q * self.total as f64).ceil() as u64).clamp(1, self.total);
        let mut seen = 0;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= wanted {
                return highest(index).min(self.max).max(self.min);
            }
        }
        self.max
    }

    /// Every slot that holds something, as the lowest value of the slot, the highest, and how many
    /// values it holds, from the smallest up.
    ///
    /// This is what a report writes out, because a histogram with a few hundred occupied slots is a
    /// few hundred rows and the 3,968 empty ones say nothing.
    pub fn buckets(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, &count)| (lowest(index), highest(index), count))
    }
}

#[cfg(test)]
mod tests {
    use super::{HIGHEST, Histogram, LEN, highest, lowest, slot};

    #[test]
    fn the_layout_is_the_one_the_spec_sized() {
        // 30 buckets, the first whole and the other 29 by their upper half, 31 KiB of counts.
        assert_eq!(LEN, 3_968);
        assert!(slot(HIGHEST) < LEN);
    }

    #[test]
    fn small_values_are_kept_exactly() {
        for value in 0..=255 {
            assert_eq!(lowest(slot(value)), value);
            assert_eq!(highest(slot(value)), value);
        }
    }

    #[test]
    fn every_slot_holds_the_values_it_says_and_no_others() {
        // Walking the slots in order must tile the number line with no gap and no overlap, which
        // is the property that makes the index and its inverse agree.
        let mut next = 0;
        for index in 0..=slot(HIGHEST) {
            assert_eq!(lowest(index), next, "slot {index}");
            assert!(highest(index) >= lowest(index));
            assert_eq!(slot(lowest(index)), index);
            assert_eq!(slot(highest(index)), index);
            next = highest(index) + 1;
        }
    }

    #[test]
    fn the_relative_error_is_under_one_percent_everywhere() {
        let mut value = 1u64;
        while value <= HIGHEST {
            let index = slot(value);
            let width = highest(index) - lowest(index);
            assert!(width * 100 <= lowest(index).max(1), "value {value} slot width {width}");
            value = value * 3 / 2 + 1;
        }
    }

    #[test]
    fn quantiles_of_a_uniform_run_land_within_the_error() {
        let mut h = Histogram::new();
        for value in 1..=100_000u64 {
            h.record(value * 1_000);
        }
        assert_eq!(h.count(), 100_000);
        assert_eq!(h.min(), 1_000);
        assert_eq!(h.max(), 100_000_000);
        for (q, exact) in
            [(0.5, 50_000_000u64), (0.9, 90_000_000), (0.99, 99_000_000), (0.999, 99_900_000)]
        {
            let got = h.quantile(q);
            assert!(got >= exact, "q {q} got {got} under {exact}");
            assert!(got - exact <= exact / 100, "q {q} got {got} against {exact}");
        }
        assert_eq!(h.quantile(1.0), 100_000_000);
        assert_eq!(h.quantile(0.0), 1_000);
        assert!((h.mean() - 50_000_500.0).abs() < 1.0);
    }

    #[test]
    fn merging_two_workers_is_the_same_as_one_worker_recording_both() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        let mut both = Histogram::new();
        for value in 0..5_000u64 {
            let v = value * value * 37 + 11;
            if value % 3 == 0 {
                a.record(v);
            } else {
                b.record(v);
            }
            both.record(v);
        }
        a.merge(&b);
        assert_eq!(a, both);
    }

    #[test]
    fn a_value_past_the_top_is_counted_and_clamped_rather_than_dropped() {
        let mut h = Histogram::new();
        h.record(5);
        h.record_n(HIGHEST * 7, 3);
        assert_eq!(h.count(), 4);
        assert_eq!(h.clamped(), 3);
        assert_eq!(h.max(), HIGHEST);
        assert_eq!(h.quantile(0.99), HIGHEST);
    }

    #[test]
    fn an_empty_histogram_answers_zero() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.min(), 0);
        assert_eq!(h.max(), 0);
        assert_eq!(h.quantile(0.5), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.buckets().count(), 0);
    }

    #[test]
    fn reset_forgets_and_the_buckets_list_only_what_is_there() {
        let mut h = Histogram::new();
        h.record_n(1_000, 4);
        h.record(250_000);
        assert_eq!(h.buckets().map(|(_, _, n)| n).collect::<Vec<_>>(), vec![4, 1]);
        h.reset();
        assert_eq!(h, Histogram::new());
    }
}
