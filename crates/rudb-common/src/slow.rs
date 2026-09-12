//! How many times this thread took a path written to be correct rather than fast.
//!
//! There are two of these paths and they are the same admission. One is [`Cause::Flatten`], where
//! something was handed a compact column and asked for a plain one because it did not know how to
//! read the compact form. The others are the kernels, where a loop that has a hand written version
//! for three pairs of forms met a fourth pair and fell through to reading a value at a time. Both
//! are correct and both are the reason a query is slower than it should be, so the engine counts
//! them rather than guessing about them later.
//!
//! This lives at rank 0 because of who has to reach it. The flatten is in `rudb-vector` at rank 1,
//! the kernels are at rank 3, and the thing that has to read the number and say which operator it
//! belongs to is the instrumentation shim at rank 4. Rank 1 cannot see rank 3 and neither can see
//! rank 4, so the only place all three can see is the bottom.
//!
//! The count is per thread and it is a plain [`Cell`] rather than an atomic. Both of those are the
//! point rather than an optimisation. The shim reads the count before an operator call and after
//! it, and the difference is what that call did. With one process wide counter, two threads running
//! the same pipeline at the same time would each read the other's work into their own difference,
//! and the per operator attribution this exists to produce would be noise. A thread cannot race
//! with itself, so the number a thread reads is exactly what that thread did, and it stays exact
//! when F4 puts eight of them on the same pipeline.
//!
//! Nothing here resets itself. The counter runs for the life of the thread and every reader takes a
//! difference, because a reader that reset would be taking the count away from whoever else was
//! reading it.

use std::cell::Cell;

/// What made a call take the slow path.
///
/// Not exhaustive, because the whole reason this exists is that the list of forms is going to grow.
/// F1 adds bit packed, run length and FSST columns, and each of them arrives with its own set of
/// kernels that do not handle it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cause {
    /// A compact column was copied out into a plain one.
    Flatten,
    /// A comparison read a value at a time.
    Compare,
    /// A scalar function read a value at a time.
    Scalar,
    /// Three valued logic read a value at a time.
    Logic,
    /// A conversion read a value at a time.
    Cast,
    /// An aggregate read a value at a time.
    Aggregate,
    /// Turning a vector of flags into the rows it keeps read a value at a time.
    Select,
}

/// How many causes there are, which is how wide a [`Tally`] is.
const KINDS: usize = 7;

impl Cause {
    /// Every cause, in the order a tally prints them.
    pub const ALL: [Self; KINDS] = [
        Self::Flatten,
        Self::Compare,
        Self::Scalar,
        Self::Logic,
        Self::Cast,
        Self::Aggregate,
        Self::Select,
    ];

    /// The name in the document and in the report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Flatten => "flatten",
            Self::Compare => "compare",
            Self::Scalar => "scalar",
            Self::Logic => "logic",
            Self::Cast => "cast",
            Self::Aggregate => "aggregate",
            Self::Select => "select",
        }
    }

    /// Where this cause sits in an array with one slot per cause.
    ///
    /// Public because a tally is not the only thing that wants one slot per cause. The operator
    /// counters keep an atomic per cause and index it with this, which beats a map on a path that
    /// is taken once per chunk.
    #[must_use]
    pub const fn slot(self) -> usize {
        match self {
            Self::Flatten => 0,
            Self::Compare => 1,
            Self::Scalar => 2,
            Self::Logic => 3,
            Self::Cast => 4,
            Self::Aggregate => 5,
            Self::Select => 6,
        }
    }
}

/// How many slow paths of each kind were taken.
///
/// Copy, and small enough that copying it is cheaper than borrowing it. The shim holds one of these
/// across an operator call and the document holds one per operator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    counts: [u64; KINDS],
}

impl Tally {
    /// No slow path taken at all, which is what every operator is meant to end up reporting.
    #[must_use]
    pub const fn none() -> Self {
        Self { counts: [0; KINDS] }
    }

    /// How many of this one.
    #[must_use]
    pub const fn get(&self, cause: Cause) -> u64 {
        self.counts[cause.slot()]
    }

    /// All of them.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.iter().fold(0, |sum, count| sum.saturating_add(*count))
    }

    /// Whether nothing fell back at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Every cause that happened at least once, in the order [`Cause::ALL`] lists them.
    ///
    /// Listed in a fixed order rather than largest first, because this is what the document is
    /// written from and a document whose keys move around depending on the numbers is a document
    /// that is hard to diff. Whoever wants the largest asks [`Self::worst`] for it.
    pub fn taken(&self) -> impl Iterator<Item = (Cause, u64)> + '_ {
        Cause::ALL.into_iter().map(|cause| (cause, self.get(cause))).filter(|(_, seen)| *seen > 0)
    }

    /// The cause with the most against it, or none if nothing fell back.
    ///
    /// Ties go to whichever comes first in [`Cause::ALL`], which makes the answer stable across
    /// runs. A tie between two causes is not a case anybody is deciding anything from anyway.
    #[must_use]
    pub fn worst(&self) -> Option<(Cause, u64)> {
        self.taken().max_by_key(|(cause, seen)| (*seen, std::cmp::Reverse(cause.slot())))
    }

    /// What happened between `before` and this reading.
    ///
    /// Saturating, so a reader that somehow gets the two the wrong way round reports nothing rather
    /// than reporting a number near `u64::MAX`.
    #[must_use]
    pub fn since(&self, before: Self) -> Self {
        let mut counts = [0; KINDS];
        for (slot, (now, then)) in
            counts.iter_mut().zip(self.counts.iter().zip(before.counts.iter()))
        {
            *slot = now.saturating_sub(*then);
        }
        Self { counts }
    }

    /// Adds another tally into this one.
    pub fn add(&mut self, other: Self) {
        for (slot, more) in self.counts.iter_mut().zip(other.counts.iter()) {
            *slot = slot.saturating_add(*more);
        }
    }

    /// A tally with one count in it, for a caller that has a number rather than a running total.
    #[must_use]
    pub fn of(cause: Cause, times: u64) -> Self {
        let mut tally = Self::none();
        tally.counts[cause.slot()] = times;
        tally
    }
}

thread_local! {
    /// What this thread has fallen back to so far.
    static TAKEN: Cell<Tally> = const { Cell::new(Tally::none()) };
}

/// Records that this thread took a slow path.
pub fn took(cause: Cause) {
    took_many(cause, 1);
}

/// Records that this thread took a slow path more than once.
///
/// For a caller that does the falling back in a loop it wrote itself and would rather add at the
/// end than on every turn.
pub fn took_many(cause: Cause, times: u64) {
    TAKEN.with(|taken| {
        let mut tally = taken.get();
        tally.add(Tally::of(cause, times));
        taken.set(tally);
    });
}

/// What this thread has fallen back to so far, for taking a difference against later.
#[must_use]
pub fn here() -> Tally {
    TAKEN.with(Cell::get)
}

/// Sets this thread's count back to nothing.
///
/// For tests, and for a harness that runs one query per thread and would rather read a total than
/// take a difference. Everything inside the engine takes a difference.
pub fn reset() {
    TAKEN.with(|taken| taken.set(Tally::none()));
}

#[cfg(test)]
mod tests {
    use super::{Cause, Tally, here, reset, took, took_many};

    #[test]
    fn a_fall_back_lands_against_its_own_cause_and_leaves_the_rest_alone() {
        reset();
        took(Cause::Flatten);
        took(Cause::Flatten);
        took(Cause::Compare);
        let tally = here();
        assert_eq!(tally.get(Cause::Flatten), 2);
        assert_eq!(tally.get(Cause::Compare), 1);
        assert_eq!(tally.get(Cause::Cast), 0);
        assert_eq!(tally.total(), 3);
        reset();
    }

    #[test]
    fn a_difference_is_what_happened_between_the_two_readings_and_nothing_before_them() {
        reset();
        took_many(Cause::Cast, 5);
        let before = here();
        took(Cause::Select);
        took(Cause::Select);
        let during = here().since(before);
        assert_eq!(during.get(Cause::Select), 2);
        assert_eq!(during.get(Cause::Cast), 0, "what happened before the reading is not in it");
        assert_eq!(during.total(), 2);
        reset();
    }

    #[test]
    fn a_difference_taken_backwards_reports_nothing_rather_than_an_enormous_number() {
        let later = Tally::of(Cause::Logic, 3);
        assert!(Tally::none().since(later).is_empty());
    }

    #[test]
    fn the_worst_cause_is_the_one_to_go_and_write_a_specialisation_for() {
        let mut tally = Tally::of(Cause::Flatten, 2);
        tally.add(Tally::of(Cause::Scalar, 90));
        tally.add(Tally::of(Cause::Logic, 11));
        assert_eq!(tally.worst(), Some((Cause::Scalar, 90)));
        assert_eq!(tally.taken().count(), 3);
        assert_eq!(Tally::none().worst(), None);
    }

    #[test]
    fn the_causes_are_listed_in_one_order_however_big_the_numbers_are() {
        let mut tally = Tally::of(Cause::Select, 1);
        tally.add(Tally::of(Cause::Flatten, 1000));
        let listed: Vec<&str> = tally.taken().map(|(cause, _)| cause.name()).collect();
        assert_eq!(listed, ["flatten", "select"]);
    }

    #[test]
    fn one_thread_counting_is_invisible_to_another() {
        reset();
        took_many(Cause::Aggregate, 4);
        let elsewhere = std::thread::spawn(|| {
            took(Cause::Aggregate);
            here()
        })
        .join()
        .expect("no counting thread panics");
        assert_eq!(elsewhere.get(Cause::Aggregate), 1, "the other thread starts from nothing");
        assert_eq!(here().get(Cause::Aggregate), 4, "and does not add to this one");
        reset();
    }

    #[test]
    fn every_cause_has_its_own_slot_and_its_own_name() {
        let mut seen: Vec<&str> = Cause::ALL.iter().map(|cause| cause.name()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), Cause::ALL.len());
        for cause in Cause::ALL {
            assert_eq!(Tally::of(cause, 7).total(), 7);
            assert_eq!(Tally::of(cause, 7).get(cause), 7);
        }
    }
}
