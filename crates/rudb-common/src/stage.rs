//! Where an operator's time went, split by the phase inside it that spent it.
//!
//! A ClickBench run says the file scan is more than half of everything the engine charges, and one
//! number for a scan is not a number anybody can act on. A scan reads bytes off a file, hands them
//! to a codec, decodes a page into values, builds a dictionary and copies pieces of pages into the
//! chunk an operator sees. Those are five different pieces of code with five different fixes, and
//! working on any of them without knowing which one holds the time is guessing.
//!
//! A grouped aggregate is the same story. It folds rows into a hash table, splits that table across
//! radix partitions, merges one instance's table into another and turns the finished tables into
//! rows, and on ClickBench at a million rows the last two are a third of the query and neither of
//! them showed up anywhere. The threads that do them are started by the aggregate rather than taken
//! from the pool, so their CPU reaches neither the pipeline counters nor the worker total, and the
//! only trace they left was wall time nobody could account for.
//!
//! This is [`crate::slow`] with a clock instead of a count, and it is here for the same reason that
//! one is here. The stages happen in `rudb-parquet` at rank 5, the thing that has to say which
//! operator they belong to is the instrumentation shim in `rudb-pipeline` at rank 4, and neither can
//! see the other. The bottom is where both can reach.
//!
//! Per thread and a plain [`Cell`], again for the reason that one is. The shim takes a reading
//! before an operator call and after it and the difference is what that call did, which is only true
//! if no other thread is counting into the same place. F4 puts several threads on one scan and this
//! keeps meaning the same thing on the day it does.
//!
//! The clock runs once per page and once per chunk, never once per value. A page is thousands of
//! values, so a pair of clock readings around it is not measurable next to what it measures. A pair
//! of readings per value would be the measurement rather than the thing measured.

use std::cell::Cell;
use std::time::Instant;

/// A named phase inside one operator, small enough that knowing it holds the time says what to fix.
///
/// The first five are the stages of reading a column, in the order the bytes go through them. The
/// rest are the phases of a grouped aggregate, which needs the same split for the same reason: one
/// number for an aggregate says it is slow and nothing about which of folding rows, splitting a
/// table by radix bits, merging one instance's table into another or turning a finished table into
/// rows is the part that is slow.
///
/// Not exhaustive because an operator this does not measure yet has phases this list does not name,
/// and one added later should be able to say where its time went without every match on this
/// breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Stage {
    /// Getting the bytes off the file, which is the part an operating system does.
    Read,
    /// Turning the compressed body of a page into its bytes.
    Decompress,
    /// Turning the bytes of a page into values, levels included.
    Decode,
    /// Building the dictionary a chunk's pages refer to.
    Dictionary,
    /// Cutting pages to the chunk boundary and putting the columns side by side.
    Assemble,
    /// Getting the room an operator is about to fill, which is the allocation and the zeroing.
    ///
    /// Separate from the stage that fills it because the two have different fixes. A fold that is
    /// slow wants a better probe and a reserve that is slow wants a buffer that is kept rather than
    /// made again, and a number that adds them together says neither.
    Reserve,
    /// Folding a chunk of rows into a hash table, which is the probe and the accumulator update.
    Fold,
    /// Splitting a table across the radix partitions, or folding rows straight into them.
    Scatter,
    /// Folding one instance's table into another, one probe per group rather than per row.
    Merge,
    /// Counting groups after a grouped distinct pass has discarded duplicate pairs.
    Count,
    /// Turning a finished table into the chunks it answers for.
    Emit,
}

/// How many stages there are, which is how wide a [`Spent`] is.
const STAGES: usize = 11;

impl Stage {
    /// Every stage, in the order the work goes through them.
    pub const ALL: [Self; STAGES] = [
        Self::Read,
        Self::Decompress,
        Self::Decode,
        Self::Dictionary,
        Self::Assemble,
        Self::Reserve,
        Self::Fold,
        Self::Scatter,
        Self::Merge,
        Self::Count,
        Self::Emit,
    ];

    /// The name in the document and in the report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Decompress => "decompress",
            Self::Decode => "decode",
            Self::Dictionary => "dictionary",
            Self::Assemble => "assemble",
            Self::Reserve => "reserve",
            Self::Fold => "fold",
            Self::Scatter => "scatter",
            Self::Merge => "merge",
            Self::Count => "count",
            Self::Emit => "emit",
        }
    }

    /// Where this stage sits in an array with one slot per stage.
    #[must_use]
    pub const fn slot(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Decompress => 1,
            Self::Decode => 2,
            Self::Dictionary => 3,
            Self::Assemble => 4,
            Self::Reserve => 5,
            Self::Fold => 6,
            Self::Scatter => 7,
            Self::Merge => 8,
            Self::Count => 9,
            Self::Emit => 10,
        }
    }
}

/// How long each stage took and how many bytes went through it.
///
/// The bytes are here rather than worked out later because a rate is the number that says whether a
/// stage is slow. Two hundred milliseconds of decompression is a fact about a query and two hundred
/// megabytes a second is a fact about the decompressor, and only the second one can be compared
/// against anything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spent {
    nanos: [u64; STAGES],
    bytes: [u64; STAGES],
}

impl Spent {
    /// Nothing measured, which is what every operator that is not a scan reports.
    #[must_use]
    pub const fn none() -> Self {
        Self { nanos: [0; STAGES], bytes: [0; STAGES] }
    }

    /// How long this stage took.
    #[must_use]
    pub const fn nanos(&self, stage: Stage) -> u64 {
        self.nanos[stage.slot()]
    }

    /// How many bytes went through it.
    #[must_use]
    pub const fn bytes(&self, stage: Stage) -> u64 {
        self.bytes[stage.slot()]
    }

    /// Every stage, added up.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.nanos.iter().fold(0, |sum, nanos| sum.saturating_add(*nanos))
    }

    /// Whether no stage recorded anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nanos.iter().all(|nanos| *nanos == 0) && self.bytes.iter().all(|bytes| *bytes == 0)
    }

    /// Every stage that did something, in the order [`Stage::ALL`] lists them.
    ///
    /// The order is the order the bytes go through the stages rather than largest first, because
    /// this is what the document is written from and a document whose keys move with its numbers is
    /// one nobody can diff. Whoever wants the largest asks [`Self::worst`].
    pub fn taken(&self) -> impl Iterator<Item = (Stage, u64, u64)> + '_ {
        Stage::ALL
            .into_iter()
            .map(|stage| (stage, self.nanos(stage), self.bytes(stage)))
            .filter(|(_, nanos, bytes)| *nanos > 0 || *bytes > 0)
    }

    /// The stage holding the most time, or none if nothing was measured.
    #[must_use]
    pub fn worst(&self) -> Option<(Stage, u64)> {
        self.taken()
            .map(|(stage, nanos, _)| (stage, nanos))
            .filter(|(_, nanos)| *nanos > 0)
            .max_by_key(|(stage, nanos)| (*nanos, std::cmp::Reverse(stage.slot())))
    }

    /// What happened between `before` and this reading.
    ///
    /// Saturating, so a reader that takes the two the wrong way round reports nothing rather than
    /// most of a century.
    #[must_use]
    pub fn since(&self, before: Self) -> Self {
        let mut out = Self::none();
        for slot in 0..STAGES {
            out.nanos[slot] = self.nanos[slot].saturating_sub(before.nanos[slot]);
            out.bytes[slot] = self.bytes[slot].saturating_sub(before.bytes[slot]);
        }
        out
    }

    /// Adds another reading into this one.
    pub fn add(&mut self, other: Self) {
        for slot in 0..STAGES {
            self.nanos[slot] = self.nanos[slot].saturating_add(other.nanos[slot]);
            self.bytes[slot] = self.bytes[slot].saturating_add(other.bytes[slot]);
        }
    }

    /// One stage's worth, for a caller that has a number rather than a running total.
    #[must_use]
    pub fn of(stage: Stage, nanos: u64, bytes: u64) -> Self {
        let mut spent = Self::none();
        spent.nanos[stage.slot()] = nanos;
        spent.bytes[stage.slot()] = bytes;
        spent
    }
}

thread_local! {
    /// What this thread has spent in each stage so far.
    static SPENT: Cell<Spent> = const { Cell::new(Spent::none()) };
}

/// Records time and bytes against a stage on this thread.
pub fn took(stage: Stage, nanos: u64, bytes: u64) {
    SPENT.with(|spent| {
        let mut now = spent.get();
        now.add(Spent::of(stage, nanos, bytes));
        spent.set(now);
    });
}

/// Adds what another thread spent to this thread's total.
///
/// For work an operator hands to threads of its own rather than to the pool. The instrumentation
/// shim takes its reading on the thread that called the operator, so a thread the operator started
/// is invisible to it, and the aggregate's finalize is exactly that: it closes sixteen partitions
/// on threads it scopes itself and then joins them. Each of those threads reads its own total when
/// it finishes and the one that started them adds the readings here, so the phases come out against
/// the operator that did them and nothing is lost.
///
/// The time is a sum over threads and not an elapsed time, the same as every other stage number,
/// because that is the one that compares against the CPU an operator charged.
pub fn gained(spent: Spent) {
    SPENT.with(|slot| {
        let mut now = slot.get();
        now.add(spent);
        slot.set(now);
    });
}

/// What this thread has spent so far, for taking a difference against later.
#[must_use]
pub fn here() -> Spent {
    SPENT.with(Cell::get)
}

/// Sets this thread's reading back to nothing.
///
/// For tests, and for a harness that runs one query per thread. Everything inside the engine takes
/// a difference instead.
pub fn reset() {
    SPENT.with(|spent| spent.set(Spent::none()));
}

/// A clock started at one stage, charging what it measured when it stops.
///
/// The pair of calls is a type rather than two lines because the second line is the one that gets
/// forgotten, and a stage that starts a clock and never stops it is a stage that reads as free.
#[derive(Debug)]
pub struct Timing {
    stage: Stage,
    at: Instant,
}

impl Timing {
    /// Starts the clock for a stage.
    #[must_use]
    pub fn start(stage: Stage) -> Self {
        Self { stage, at: Instant::now() }
    }

    /// Stops it and charges the time, along with the bytes that went through.
    ///
    /// A caller with no meaningful byte count passes zero, which keeps the stage out of the rate
    /// column rather than putting a nought in it.
    pub fn stop(self, bytes: u64) {
        let nanos = u64::try_from(self.at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        took(self.stage, nanos, bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{Spent, Stage, here, reset, took};

    #[test]
    fn time_lands_against_its_own_stage_and_leaves_the_rest_alone() {
        reset();
        took(Stage::Read, 100, 4096);
        took(Stage::Read, 50, 1024);
        took(Stage::Decompress, 700, 8192);
        let spent = here();
        assert_eq!(spent.nanos(Stage::Read), 150);
        assert_eq!(spent.bytes(Stage::Read), 5120);
        assert_eq!(spent.nanos(Stage::Decompress), 700);
        assert_eq!(spent.nanos(Stage::Decode), 0);
        assert_eq!(spent.total(), 850);
        reset();
    }

    #[test]
    fn a_difference_is_what_happened_between_the_two_readings_and_nothing_before_them() {
        reset();
        took(Stage::Decode, 900, 16);
        let before = here();
        took(Stage::Assemble, 12, 0);
        let during = here().since(before);
        assert_eq!(during.nanos(Stage::Assemble), 12);
        assert_eq!(during.nanos(Stage::Decode), 0, "what happened before the reading is not in it");
        assert_eq!(during.total(), 12);
        reset();
    }

    #[test]
    fn a_difference_taken_backwards_reports_nothing_rather_than_most_of_a_century() {
        let later = Spent::of(Stage::Read, 900, 900);
        assert!(Spent::none().since(later).is_empty());
    }

    #[test]
    fn the_worst_stage_is_the_one_worth_working_on() {
        let mut spent = Spent::of(Stage::Read, 40, 0);
        spent.add(Spent::of(Stage::Decompress, 4000, 0));
        spent.add(Spent::of(Stage::Decode, 900, 0));
        assert_eq!(spent.worst(), Some((Stage::Decompress, 4000)));
        assert_eq!(spent.taken().count(), 3);
        assert_eq!(Spent::none().worst(), None);
    }

    #[test]
    fn a_stage_that_only_moved_bytes_is_listed_and_is_not_the_worst() {
        let mut spent = Spent::of(Stage::Read, 0, 8192);
        spent.add(Spent::of(Stage::Decode, 5, 0));
        let listed: Vec<&str> = spent.taken().map(|(stage, _, _)| stage.name()).collect();
        assert_eq!(listed, ["read", "decode"]);
        assert_eq!(spent.worst(), Some((Stage::Decode, 5)));
    }

    #[test]
    fn one_thread_timing_is_invisible_to_another() {
        reset();
        took(Stage::Dictionary, 44, 0);
        let elsewhere = std::thread::spawn(|| {
            took(Stage::Dictionary, 1, 0);
            here()
        })
        .join()
        .expect("no timing thread panics");
        assert_eq!(elsewhere.nanos(Stage::Dictionary), 1, "the other thread starts from nothing");
        assert_eq!(here().nanos(Stage::Dictionary), 44, "and does not add to this one");
        reset();
    }

    #[test]
    fn every_stage_has_its_own_slot_and_its_own_name() {
        let mut seen: Vec<&str> = Stage::ALL.iter().map(|stage| stage.name()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), Stage::ALL.len());
        for stage in Stage::ALL {
            assert_eq!(Spent::of(stage, 7, 3).total(), 7);
            assert_eq!(Spent::of(stage, 7, 3).bytes(stage), 3);
        }
    }
}
