//! The durable cut, `09-the-log.md` section 9.7.
//!
//! With more than one lane a commit's lane can sync before the lane of a commit it was built on,
//! and acknowledging it then would let a crash keep it and lose what it read. Every commit carries
//! `dep_ts`, the newest commit it depends on, and is acknowledged once its own lane has it and the
//! cut has reached its `dep_ts`. The cut is the largest timestamp at or below which every commit is
//! as far as its `commit_sync` asked, and since timestamps are dense a commit that is not there yet
//! is a gap the cut stops at, without any lane telling the others where it is.
//!
//! The spec keeps the commits above the cut in a ring of bits advanced by compare and swap. This
//! is the same under a lock: commits are few enough per sync that the lock is never what a
//! committer waits on, and the waiters park on its condition variable.

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use rudb_common::{Error, Result};

/// The durable cut every lane of one log shares.
#[derive(Debug, Default)]
pub struct Cut {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Every commit at or below it is there.
    at: u64,
    /// Commits above it that are there, past a gap.
    above: BTreeSet<u64>,
    /// Whether a lane failed, after which nothing waiting on the cut can be told it is there
    /// until a checkpoint writes everything into the file.
    failed: bool,
}

impl Cut {
    /// A cut at `at`, where every commit is already in the file.
    #[must_use]
    pub fn new(at: u64) -> Self {
        Self { state: Mutex::new(State { at, ..State::default() }), changed: Condvar::new() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the cut is.
    #[must_use]
    pub fn at(&self) -> u64 {
        self.lock().at
    }

    /// Notes that the commit at `ts` is there, and moves the cut over every commit after it that
    /// is too.
    pub fn reached(&self, ts: u64) {
        let mut state = self.lock();
        if ts <= state.at {
            return;
        }
        state.above.insert(ts);
        let before = state.at;
        while let Some(next) = state.at.checked_add(1)
            && state.above.remove(&next)
        {
            state.at = next;
        }
        if state.at != before {
            self.changed.notify_all();
        }
    }

    /// Moves the cut to `ts` at least, for a checkpoint that wrote every commit up to it into the
    /// file, which also clears a failure: what the failed lane held is in the file now.
    pub fn raise(&self, ts: u64) {
        let mut state = self.lock();
        state.failed = false;
        if ts > state.at {
            state.at = ts;
            state.above = state.above.split_off(&ts);
            state.above.remove(&ts);
        }
        while let Some(next) = state.at.checked_add(1)
            && state.above.remove(&next)
        {
            state.at = next;
        }
        self.changed.notify_all();
    }

    /// Notes that a lane failed, which wakes every commit waiting on the cut to fail too.
    pub fn fail(&self) {
        self.lock().failed = true;
        self.changed.notify_all();
    }

    /// Waits until the cut reaches `ts`.
    ///
    /// # Errors
    ///
    /// If a lane failed before it did.
    pub fn wait(&self, ts: u64) -> Result<()> {
        let mut state = self.lock();
        loop {
            if state.at >= ts {
                return Ok(());
            }
            if state.failed {
                return Err(Error::io(
                    "a log lane failed before a commit this one depends on was durable",
                ));
            }
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Cut;

    #[test]
    fn the_cut_stops_at_a_gap_and_moves_over_it_once_filled() {
        let cut = Cut::new(10);
        cut.reached(12);
        cut.reached(13);
        assert_eq!(cut.at(), 10);
        cut.reached(11);
        assert_eq!(cut.at(), 13);
        cut.reached(9);
        cut.reached(15);
        assert_eq!(cut.at(), 13);
        cut.raise(14);
        assert_eq!(cut.at(), 15);
        cut.raise(12);
        assert_eq!(cut.at(), 15);
    }

    #[test]
    fn a_waiter_wakes_when_the_cut_reaches_it_or_a_lane_fails() {
        let cut = Arc::new(Cut::new(0));
        let waiter = {
            let cut = Arc::clone(&cut);
            std::thread::spawn(move || cut.wait(2))
        };
        cut.reached(2);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!waiter.is_finished());
        cut.reached(1);
        waiter.join().expect("joins").expect("reached");
        let waiter = {
            let cut = Arc::clone(&cut);
            std::thread::spawn(move || cut.wait(3))
        };
        cut.fail();
        assert!(waiter.join().expect("joins").is_err());
        cut.raise(3);
        cut.wait(3).expect("raised");
    }
}
