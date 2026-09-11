//! How much memory a query is allowed to hold.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// A budget shared by everything running against one database.
///
/// Cheap to clone, and a clone shares the total with the budget it came from, so two queries running
/// at once are held to one limit between them rather than to one each. That is what DuckDB's
/// `memory_limit` means and it is the only reading that is any use: a limit that each query gets a
/// fresh copy of is not a limit on the process.
///
/// # What it counts
///
/// What an operator says it is holding. Nothing here hooks the allocator, so the number is the sum
/// of what the buffering operators reserved and not the resident size of the process. The gap is
/// real and it is in one direction: a query that holds a gigabyte of rows has reserved a gigabyte,
/// and the allocator's own bookkeeping on top of that is not counted. Under reporting is the safe
/// direction for a first version, because the alternative is refusing a query that would have fit.
///
/// The operators that reserve are the ones that buffer without bound, which is sorting, grouping,
/// duplicate elimination, joining, set operations and the result a query hands back. A streaming
/// operator holds one chunk and gives it away again, so charging it would be counting the same
/// megabyte once per level of the tree.
///
/// # Why a reservation rather than a pair of calls
///
/// [`Memory::reserve`] hands back a [`Reservation`] that releases what it took when it is dropped,
/// so an operator that fails halfway through, or a query stopped by an interrupt, gives its memory
/// back without anybody writing the release. A pair of `take` and `give` calls is the version where
/// the release is missed on the error path, and the error path here is the one that matters, since
/// running out of memory is itself an error and it unwinds through every operator below.
#[derive(Debug, Clone)]
pub struct Memory {
    inner: Arc<Budget>,
}

#[derive(Debug)]
struct Budget {
    used: AtomicU64,
    limit: Option<u64>,
}

impl Default for Memory {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl Memory {
    /// A budget nothing is refused against, which still counts what is held.
    ///
    /// The counting is kept because [`Memory::used`] is worth reading whether or not there is a
    /// limit, and because a query that behaves differently depending on whether a limit is set is a
    /// query whose limit cannot be tested by setting one.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(None)
    }

    /// A budget of this many bytes.
    #[must_use]
    pub fn with_limit(bytes: u64) -> Self {
        Self::new(Some(bytes))
    }

    /// A budget of this many bytes, or no limit at all.
    #[must_use]
    pub fn new(limit: Option<u64>) -> Self {
        Self { inner: Arc::new(Budget { used: AtomicU64::new(0), limit }) }
    }

    /// The limit, if there is one.
    #[must_use]
    pub fn limit(&self) -> Option<u64> {
        self.inner.limit
    }

    /// How many bytes are held right now.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Relaxed)
    }

    /// A reservation on this budget that is holding nothing yet.
    ///
    /// What a buffering operator starts with, because it is built before it has read anything and
    /// its constructor has no error to report. It grows as the input arrives.
    #[must_use]
    pub fn reservation(&self) -> Reservation {
        Reservation { memory: self.clone(), bytes: 0 }
    }

    /// Takes `bytes` out of the budget, to be given back when the reservation is dropped.
    ///
    /// Reserving nothing always works and is the way an operator gets a handle it can grow later.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorCode::OutOfMemory`] when the limit is set and this would pass it. Nothing is
    /// taken in that case, so a caller that carries on after catching it is holding what it held
    /// before.
    pub fn reserve(&self, bytes: u64) -> Result<Reservation> {
        self.take(bytes)?;
        Ok(Reservation { memory: self.clone(), bytes })
    }

    /// Adds to the total, or reports that it cannot.
    ///
    /// The loop is a compare and exchange rather than a fetch and add with a check afterwards,
    /// because a fetch and add that has to be undone is a window in which another thread sees a
    /// total that was never allowed and refuses a query that would have fit.
    fn take(&self, bytes: u64) -> Result<()> {
        let Some(limit) = self.inner.limit else {
            self.inner.used.fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        };
        let mut used = self.inner.used.load(Ordering::Relaxed);
        loop {
            let wanted = used.saturating_add(bytes);
            if wanted > limit {
                return Err(Error::out_of_memory(format!(
                    "could not allocate {} ({}/{} used)",
                    human(bytes),
                    human(used),
                    human(limit)
                )));
            }
            match self.inner.used.compare_exchange_weak(
                used,
                wanted,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(now) => used = now,
            }
        }
    }

    /// Gives bytes back.
    fn give(&self, bytes: u64) {
        self.inner.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Memory one operator is holding, given back when this is dropped.
///
/// It starts at whatever [`Memory::reserve`] was asked for and grows from there, which is the shape
/// a buffering operator wants: it does not know how much it will hold until it has read its input,
/// and it wants to be told as soon as the answer is too much rather than after the last row.
#[derive(Debug)]
pub struct Reservation {
    memory: Memory,
    bytes: u64,
}

impl Reservation {
    /// How much this reservation is holding.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Takes another `bytes` out of the same budget.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorCode::OutOfMemory`] when the limit is set and this would pass it. The
    /// reservation is unchanged in that case and still releases what it already held.
    pub fn grow(&mut self, bytes: u64) -> Result<()> {
        self.memory.take(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// Gives everything back now rather than at the end of the scope.
    ///
    /// For an operator that has finished with its buffer and is about to hand out what it built
    /// from it, where waiting for the drop would hold two copies against the limit at once.
    pub fn release(&mut self) {
        self.memory.give(self.bytes);
        self.bytes = 0;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.memory.give(self.bytes);
    }
}

/// A size the way an error message says one.
///
/// The shape DuckDB prints, which is one decimal place and the binary units, so that
/// `9.3 MiB/9.5 MiB used` in a message from here reads as the same sentence as the one from there.
/// It rounds, which is why it is not the formatter `--print-config` uses: a configuration dump has
/// to print a number somebody can compare against what they set, and a message about running out of
/// memory has to print one somebody can read.
fn human(bytes: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "a rounded size is the point of this function")]
    let mut size = bytes as f64;
    for unit in ["bytes", "KiB", "MiB", "GiB", "TiB"] {
        if size < 1024.0 || unit == "TiB" {
            return if unit == "bytes" {
                format!("{bytes} bytes")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1024.0;
    }
    unreachable!("the loop returns on its last unit")
}

#[cfg(test)]
mod tests {
    use super::{Memory, human};

    #[test]
    fn an_unlimited_budget_refuses_nothing_and_still_counts() {
        let memory = Memory::unlimited();
        assert_eq!(memory.limit(), None);
        let held = memory.reserve(1 << 30).expect("nothing is refused");
        assert_eq!(memory.used(), 1 << 30);
        assert_eq!(held.bytes(), 1 << 30);
    }

    #[test]
    fn a_reservation_gives_its_bytes_back_when_it_is_dropped() {
        let memory = Memory::with_limit(1024);
        {
            let _held = memory.reserve(1000).expect("a thousand of a thousand and twenty four");
            assert_eq!(memory.used(), 1000);
        }
        assert_eq!(memory.used(), 0);
        memory.reserve(1000).expect("the room is back");
    }

    #[test]
    fn passing_the_limit_is_an_out_of_memory_error_that_says_the_numbers() {
        let memory = Memory::with_limit(10 * 1024 * 1024);
        let _held = memory.reserve(9 * 1024 * 1024).expect("nine of ten");
        let error = memory.reserve(2 * 1024 * 1024).expect_err("eleven of ten");
        assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
        assert_eq!(error.message(), "could not allocate 2.0 MiB (9.0 MiB/10.0 MiB used)");
    }

    #[test]
    fn a_refused_reservation_takes_nothing() {
        let memory = Memory::with_limit(100);
        memory.reserve(200).expect_err("twice the limit");
        assert_eq!(memory.used(), 0);
        memory.reserve(100).expect("the limit is still all there");
    }

    #[test]
    fn a_reservation_grows_until_it_cannot() {
        let memory = Memory::with_limit(100);
        let mut held = memory.reserve(0).expect("nothing is always available");
        held.grow(60).expect("sixty of a hundred");
        held.grow(40).expect("and the other forty");
        held.grow(1).expect_err("there is no more");
        assert_eq!(held.bytes(), 100, "the refused growth changed nothing");
        assert_eq!(memory.used(), 100);
    }

    #[test]
    fn releasing_early_frees_the_room_before_the_scope_ends() {
        let memory = Memory::with_limit(100);
        let mut held = memory.reserve(100).expect("all of it");
        held.release();
        assert_eq!(memory.used(), 0);
        assert_eq!(held.bytes(), 0);
        // And the drop that follows does not take the total below zero.
        drop(held);
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn two_handles_on_one_budget_are_held_to_it_between_them() {
        // A limit each query gets a fresh copy of is not a limit on the process.
        let memory = Memory::with_limit(100);
        let other = memory.clone();
        let _held = memory.reserve(60).expect("sixty");
        other.reserve(60).expect_err("the other sixty does not fit beside it");
    }

    #[test]
    fn a_size_reads_the_way_duckdb_writes_one() {
        assert_eq!(human(0), "0 bytes");
        assert_eq!(human(512), "512 bytes");
        assert_eq!(human(256 * 1024), "256.0 KiB");
        assert_eq!(human(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(human(9_751_000), "9.3 MiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human(5 * 1024u64.pow(5)), "5120.0 TiB");
    }
}
