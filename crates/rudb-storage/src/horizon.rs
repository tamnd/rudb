//! The snapshots workers are reading at, per table, `engine-v4/08-concurrency.md` section 8.9.
//!
//! Every worker publishes the snapshot of the statement it is running, or none when idle, in a
//! slot of its own, with the tables the statement reads or writes. The horizon of a table is the oldest
//! snapshot any worker reading it holds, or the last commit when none does, and an undo record of
//! that table committed at or below it is one no reader can need. A query that reads `orderline`
//! and `item` does not hold back the undo of `district`.
//!
//! A transaction of several statements can read a table its first statement did not name, at the
//! snapshot it already has, so it publishes for every table.
//!
//! The slots also carry an epoch, which is what lets the collector free memory a reader may still
//! be looking at. A statement takes the epoch as it begins, and the collector advances it after it
//! cuts records out of their chains: a chunk cut at epoch `e` can be made again once every
//! statement that took `e` or earlier and reads a table of the chunk has ended, see
//! [`Horizons::passed`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// What a slot holds when its worker is idle.
const IDLE: u64 = u64::MAX;

/// The tables a statement reads or writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reads {
    /// Every table, for a transaction that may read one it has not named yet.
    All,
    /// These tables, from the plan.
    Tables(Vec<u32>),
}

impl Reads {
    fn holds(&self, table: u32) -> bool {
        match self {
            Self::All => true,
            Self::Tables(tables) => tables.contains(&table),
        }
    }

    fn meets(&self, tables: &[u32]) -> bool {
        tables.iter().any(|&table| self.holds(table))
    }
}

/// One worker's slot, a cache line of its own so workers beginning statements do not share one.
#[derive(Debug)]
#[repr(align(64))]
struct Slot {
    /// The snapshot it published, or [`IDLE`].
    snapshot: AtomicU64,
    /// The epoch its statement began at, or [`IDLE`].
    epoch: AtomicU64,
    /// The tables it reads, set before the snapshot is published.
    reads: Mutex<Reads>,
}

/// The slots of every worker.
#[derive(Debug)]
pub struct Horizons {
    slots: Box<[Slot]>,
    epoch: AtomicU64,
}

impl Horizons {
    /// Slots for `workers` workers, all idle.
    #[must_use]
    pub fn new(workers: usize) -> Self {
        Self {
            slots: (0..workers)
                .map(|_| Slot {
                    snapshot: AtomicU64::new(IDLE),
                    epoch: AtomicU64::new(IDLE),
                    reads: Mutex::new(Reads::All),
                })
                .collect(),
            epoch: AtomicU64::new(0),
        }
    }

    /// Begins a statement on `worker` reading or writing `reads` and returns its snapshot, the last
    /// commit timestamp `clock` held.
    ///
    /// The slot gets a value read before it is published and the statement one read after, so a
    /// collector that missed the slot read `clock` before the statement did and its horizon is no
    /// later than the statement's snapshot.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    pub fn begin(&self, worker: usize, clock: &AtomicU64, reads: Reads) -> u64 {
        let slot = &self.slots[worker];
        *slot.reads.lock().unwrap_or_else(PoisonError::into_inner) = reads;
        slot.epoch.store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        slot.snapshot.store(clock.load(Ordering::SeqCst), Ordering::SeqCst);
        clock.load(Ordering::SeqCst)
    }

    /// Begins a statement of a transaction that already has its snapshot, which keeps holding
    /// back every table. Between statements the transaction holds no undo record, so the epoch
    /// moves on and the chunks it was keeping can be freed.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    pub fn resume(&self, worker: usize, snapshot: u64) {
        let slot = &self.slots[worker];
        *slot.reads.lock().unwrap_or_else(PoisonError::into_inner) = Reads::All;
        slot.epoch.store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        slot.snapshot.store(snapshot, Ordering::SeqCst);
    }

    /// Ends the statement on `worker`.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    pub fn end(&self, worker: usize) {
        let slot = &self.slots[worker];
        slot.snapshot.store(IDLE, Ordering::SeqCst);
        slot.epoch.store(IDLE, Ordering::SeqCst);
    }

    /// The horizon of `table`: the oldest snapshot a statement reading it holds, or the last
    /// commit when none does. An undo record of the table committed at or below it is needed by
    /// nobody.
    #[must_use]
    pub fn oldest(&self, clock: &AtomicU64, table: u32) -> u64 {
        let mut oldest = clock.load(Ordering::SeqCst);
        for slot in &self.slots {
            let snapshot = slot.snapshot.load(Ordering::SeqCst);
            if snapshot < oldest
                && slot.reads.lock().unwrap_or_else(PoisonError::into_inner).holds(table)
            {
                oldest = snapshot;
            }
        }
        oldest
    }

    /// Moves the epoch on and returns the one it was, which the caller tags what it just did
    /// with.
    pub(crate) fn advance(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst)
    }

    /// Whether every statement that began at `epoch` or earlier and reads or writes one of
    /// `tables` has ended.
    ///
    /// The tables are read after the epoch, so a slot whose statement ended and whose next one
    /// began between the two is judged by the next one's tables. That is safe: the statement the
    /// epoch belonged to is over, and the next one does not touch these tables.
    #[must_use]
    pub fn passed(&self, epoch: u64, tables: &[u32]) -> bool {
        self.slots.iter().all(|slot| {
            let began = slot.epoch.load(Ordering::SeqCst);
            began == IDLE
                || began > epoch
                || !slot.reads.lock().unwrap_or_else(PoisonError::into_inner).meets(tables)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::{Horizons, Reads};

    #[test]
    fn a_table_is_held_back_only_by_the_statements_that_read_it() {
        let horizons = Horizons::new(3);
        let clock = AtomicU64::new(10);
        assert_eq!(horizons.oldest(&clock, 1), 10, "nobody reading");
        assert_eq!(horizons.begin(0, &clock, Reads::Tables(vec![1, 2])), 10);
        clock.store(20, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(horizons.begin(1, &clock, Reads::Tables(vec![3])), 20);
        assert_eq!(horizons.oldest(&clock, 1), 10);
        assert_eq!(horizons.oldest(&clock, 2), 10);
        assert_eq!(horizons.oldest(&clock, 3), 20);
        assert_eq!(horizons.oldest(&clock, 4), 20, "a table nobody reads is at the clock");
        horizons.resume(2, 5);
        assert_eq!(horizons.oldest(&clock, 4), 5, "a transaction holds back every table");
        horizons.end(2);
        horizons.end(0);
        assert_eq!(horizons.oldest(&clock, 1), 20);
    }

    #[test]
    fn an_epoch_passes_once_every_statement_begun_at_it_ends() {
        let horizons = Horizons::new(2);
        let clock = AtomicU64::new(1);
        assert!(horizons.passed(0, &[1]), "nobody is running");
        horizons.begin(0, &clock, Reads::Tables(vec![1]));
        let first = horizons.advance();
        assert!(!horizons.passed(first, &[1]), "worker 0 began at it");
        assert!(horizons.passed(first, &[2]), "and reads another table");
        horizons.begin(1, &clock, Reads::All);
        assert!(!horizons.passed(first, &[1, 2]), "worker 0 is still running");
        horizons.end(0);
        assert!(horizons.passed(first, &[1]), "worker 1 began after it");
        let second = horizons.advance();
        assert!(!horizons.passed(second, &[3]), "worker 1 began at the second, on every table");
        horizons.resume(1, 1);
        assert!(horizons.passed(second, &[3]), "its next statement began after");
    }
}
