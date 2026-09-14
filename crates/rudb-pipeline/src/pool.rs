//! How many threads a database will run queries on, and how many are spoken for right now.
//!
//! The thread count belongs to the database and not to the query. Two connections running two
//! queries on a machine with sixteen cores should use sixteen threads between them and not sixteen
//! each, and a pool that is made when a query starts cannot know about the other one. So there is
//! one of these per database and every query asks it.
//!
//! # Why it lends rather than runs
//!
//! The usual thread pool owns parked workers and is handed closures to run. That shape needs the
//! closure to outlive the call that submitted it as far as the compiler is concerned, and an
//! operator here borrows the plan and the catalog, so the usual shape needs the lifetime erased and
//! erasing a lifetime needs `unsafe`. This workspace forbids it, and a pool that made us give that
//! up would be paying for thread creation with the one property that makes a data race a compile
//! error rather than a Tuesday.
//!
//! So the pool lends a number and the driver spawns scoped threads against it. What that costs is a
//! thread creation per pipeline per run, which is tens of microseconds, and what it buys is that a
//! borrowed pipeline is checked rather than promised. The first measurement decides whether that
//! trade needs revisiting: if the per query floor moves, the answer is a pool of real workers and
//! operators that own their data, and the driver above it does not change either way.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The thread budget of one database.
#[derive(Debug)]
pub struct Pool {
    threads: AtomicUsize,
    busy: AtomicUsize,
}

impl Pool {
    /// A pool that will lend up to `threads` at once, counting the thread that asks.
    ///
    /// Zero is read as one, because a database that may run a query on no threads at all cannot run
    /// a query, and the setting that feeds this is validated where a user sets it.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        Self { threads: AtomicUsize::new(threads.max(1)), busy: AtomicUsize::new(0) }
    }

    /// The most that may run at once.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Relaxed)
    }

    /// Change it, which is what `SET threads` does.
    ///
    /// Queries already running keep the threads they were lent. Nothing is taken back mid query,
    /// because a driver that lost a thread half way through a morsel would have to either abandon
    /// the rows it read or hold them, and neither is worth having for a setting somebody changed
    /// while a query was in flight.
    pub fn resize(&self, threads: usize) {
        self.threads.store(threads.max(1), Ordering::Relaxed);
    }

    /// Borrow up to `want` threads, counting the caller's own as one of them.
    ///
    /// Never lends fewer than the caller, so a query always runs even when every other thread is
    /// spoken for. That is the difference between a governor and a semaphore, and it is deliberate:
    /// a query that waits for a thread it is not going to get is a query that hangs, and the worst
    /// a busy pool should do is make a query serial.
    #[must_use]
    pub fn lease(&self, want: usize) -> Lease<'_> {
        let wanted = want.saturating_sub(1);
        let mut busy = self.busy.load(Ordering::Relaxed);
        loop {
            let spare = self.threads().saturating_sub(1).saturating_sub(busy);
            let taken = wanted.min(spare);
            if taken == 0 {
                return Lease { pool: self, extra: 0 };
            }
            match self.busy.compare_exchange_weak(
                busy,
                busy + taken,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Lease { pool: self, extra: taken },
                Err(seen) => busy = seen,
            }
        }
    }
}

impl Default for Pool {
    /// One thread, which is the pool a test or an embedded caller that never said otherwise gets.
    fn default() -> Self {
        Self::new(1)
    }
}

/// Threads borrowed from a pool, given back when this goes away.
#[derive(Debug)]
pub struct Lease<'a> {
    pool: &'a Pool,
    /// How many threads beyond the caller's own this lease covers.
    extra: usize,
}

impl Lease<'_> {
    /// How many instances may run at once, counting the thread that asked.
    #[must_use]
    pub fn degree(&self) -> usize {
        self.extra + 1
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.busy.fetch_sub(self.extra, Ordering::Relaxed);
    }
}
