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
//!
//! A snapshot that holds more undo than the temp directory may take is killed, `15-resources.md`
//! section 15.3: the collector marks the oldest one and from then on leaves it out of every
//! horizon. Its statement goes on reading what is left until it ends and finds out, see
//! [`Horizons::too_old`], and the epochs keep what it reads in memory until then.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use rudb_common::Error;

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
    /// The tables it reads, set before the snapshot is published, and whether the snapshot was
    /// killed.
    held: Mutex<Held>,
}

/// What a slot's lock guards.
#[derive(Debug)]
struct Held {
    reads: Reads,
    /// Set by [`Horizons::kill_oldest`] for the snapshot it found in the slot. A statement that
    /// begins with another snapshot clears it before publishing that one.
    killed: Option<Killed>,
}

impl Held {
    /// Whether the statement at `snapshot` holds back `table`.
    fn holds(&self, table: u32, snapshot: u64) -> bool {
        self.reads.holds(table) && !self.killed.is_some_and(|killed| killed.snapshot == snapshot)
    }
}

/// A snapshot the collector killed, with what the error says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Killed {
    /// The snapshot.
    pub snapshot: u64,
    /// How many commits it was behind the clock.
    pub behind: u64,
    /// The undo in memory and in the temp directory when it was killed, in bytes.
    pub held: u64,
}

impl Killed {
    /// The error the transaction gets, `15-resources.md` section 15.3.
    #[must_use]
    pub fn error(&self) -> Error {
        Error::transaction(format!(
            "snapshot too old: it was {} commits behind and held {} of undo",
            self.behind,
            rudb_common::memory::human(self.held)
        ))
    }
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
                    held: Mutex::new(Held { reads: Reads::All, killed: None }),
                })
                .collect(),
            epoch: AtomicU64::new(0),
        }
    }

    /// Begins a statement on `worker` reading or writing `reads` and returns its snapshot, the last
    /// commit timestamp `clock` held.
    ///
    /// The slot holds the snapshot itself: a value of `clock` is published and then read again,
    /// until the two agree. So a collector that missed the slot read `clock` before the statement
    /// read it the second time, and every commit it can see is at or below the snapshot. One that
    /// read the slot finds the snapshot, or a value below it that is being replaced, which holds
    /// back more and never less. [`Self::snapshots`] needs the snapshot exactly, where the horizon
    /// alone would do with any value at or below it.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    pub fn begin(&self, worker: usize, clock: &AtomicU64, reads: Reads) -> u64 {
        let slot = &self.slots[worker];
        *slot.held.lock().unwrap_or_else(PoisonError::into_inner) = Held { reads, killed: None };
        slot.epoch.store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        loop {
            let snapshot = clock.load(Ordering::SeqCst);
            slot.snapshot.store(snapshot, Ordering::SeqCst);
            if clock.load(Ordering::SeqCst) == snapshot {
                return snapshot;
            }
        }
    }

    /// Begins a statement of a transaction that already has its snapshot, which keeps holding
    /// back every table. Between statements the transaction holds no undo record, so the epoch
    /// moves on and the chunks it was keeping can be freed. A snapshot that was killed stays
    /// killed.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    pub fn resume(&self, worker: usize, snapshot: u64) {
        let slot = &self.slots[worker];
        let mut held = slot.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.reads = Reads::All;
        held.killed = held.killed.filter(|killed| killed.snapshot == snapshot);
        drop(held);
        slot.epoch.store(self.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
        slot.snapshot.store(snapshot, Ordering::SeqCst);
    }

    /// Ends the statement on `worker`. Whether its snapshot was killed is [`Self::too_old`]'s
    /// question, asked before this.
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
                && slot.held.lock().unwrap_or_else(PoisonError::into_inner).holds(table, snapshot)
            {
                oldest = snapshot;
            }
        }
        oldest
    }

    /// The snapshots of the statements reading `table`, in order, which is what interval pruning
    /// holds an undo record to, see `UndoSpace::collect`. Read after `clock`, a statement missing
    /// from them has a snapshot at or past what `clock` held.
    #[must_use]
    pub fn snapshots(&self, table: u32) -> Vec<u64> {
        let mut snapshots = self
            .slots
            .iter()
            .filter_map(|slot| {
                let snapshot = slot.snapshot.load(Ordering::SeqCst);
                (snapshot != IDLE
                    && slot
                        .held
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .holds(table, snapshot))
                .then_some(snapshot)
            })
            .collect::<Vec<_>>();
        snapshots.sort_unstable();
        snapshots
    }

    /// Kills the oldest snapshot a statement holds that is not killed already, and returns it, or
    /// `None` when nobody holds one. `held` is the undo it holds back, for the error.
    ///
    /// The slot's snapshot is read under its lock, which a statement beginning with another one
    /// takes to clear the mark before it publishes. So the mark is for the snapshot the slot holds
    /// now, or for one that has already gone and that a later statement does not inherit.
    pub fn kill_oldest(&self, clock: &AtomicU64, held: u64) -> Option<Killed> {
        let now = clock.load(Ordering::SeqCst);
        let mut oldest: Option<(usize, u64)> = None;
        for (at, slot) in self.slots.iter().enumerate() {
            let snapshot = slot.snapshot.load(Ordering::SeqCst);
            let killed = slot.held.lock().unwrap_or_else(PoisonError::into_inner).killed;
            if snapshot != IDLE
                && !killed.is_some_and(|killed| killed.snapshot == snapshot)
                && oldest.is_none_or(|(_, oldest)| snapshot < oldest)
            {
                oldest = Some((at, snapshot));
            }
        }
        let (at, _) = oldest?;
        let slot = &self.slots[at];
        let mut lock = slot.held.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = slot.snapshot.load(Ordering::SeqCst);
        if snapshot == IDLE {
            return None;
        }
        let killed = Killed { snapshot, behind: now.saturating_sub(snapshot), held };
        lock.killed = Some(killed);
        Some(killed)
    }

    /// Whether the snapshot of the statement on `worker` was killed, and why. The statement asks
    /// before it ends, and fails with [`Killed::error`] when it was, since it may have read rows
    /// the collector no longer kept for it.
    ///
    /// # Panics
    ///
    /// If `worker` has no slot.
    #[must_use]
    pub fn too_old(&self, worker: usize) -> Option<Killed> {
        let slot = &self.slots[worker];
        let killed = slot.held.lock().unwrap_or_else(PoisonError::into_inner).killed;
        let snapshot = slot.snapshot.load(Ordering::SeqCst);
        killed.filter(|killed| killed.snapshot == snapshot)
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
                || !slot.held.lock().unwrap_or_else(PoisonError::into_inner).reads.meets(tables)
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
        assert_eq!(horizons.snapshots(1), vec![5, 10]);
        assert_eq!(horizons.snapshots(3), vec![5, 20]);
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

    #[test]
    fn a_killed_snapshot_holds_nothing_back_and_its_statement_finds_out() {
        let horizons = Horizons::new(3);
        let clock = AtomicU64::new(10);
        horizons.begin(0, &clock, Reads::Tables(vec![1]));
        clock.store(20, std::sync::atomic::Ordering::SeqCst);
        horizons.begin(1, &clock, Reads::Tables(vec![1]));
        clock.store(50, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(horizons.too_old(0), None);
        let killed = horizons.kill_oldest(&clock, 3 << 30).expect("one to kill");
        assert_eq!((killed.snapshot, killed.behind), (10, 40));
        assert_eq!(
            killed.error().to_string(),
            "TransactionContext Error: snapshot too old: it was 40 commits behind and held \
             3.0 GiB of undo"
        );
        assert_eq!(horizons.oldest(&clock, 1), 20, "the killed one is left out");
        assert_eq!(horizons.snapshots(1), vec![20]);
        assert_eq!(horizons.too_old(0), Some(killed));
        assert_eq!(horizons.too_old(1), None);
        // The next kill is the next oldest, not the same one again.
        assert_eq!(horizons.kill_oldest(&clock, 0).map(|killed| killed.snapshot), Some(20));
        assert_eq!(horizons.kill_oldest(&clock, 0), None, "nobody left");

        // The transaction's next statement at the same snapshot is still killed, and a new
        // statement is not.
        horizons.end(0);
        horizons.resume(0, 10);
        assert_eq!(horizons.too_old(0), Some(killed));
        horizons.end(0);
        horizons.begin(0, &clock, Reads::All);
        assert_eq!(horizons.too_old(0), None);
        assert_eq!(horizons.oldest(&clock, 1), 50);
    }
}
