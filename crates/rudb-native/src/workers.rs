//! The threads a load encodes, merges and closes on, kept for the life of the process.
//!
//! Every stage that spreads a stripe over threads used to start them with `std::thread::scope`,
//! one set for the columns of each stripe, one for the merge and one for the pages, and then more
//! at the close. The 10m ClickBench load on the 32 core box started 2,300 threads that way. The
//! threads were cheap enough, but the memory they left behind was not. mimalloc gives every thread
//! its own heap, a thread that exits hands its segments back as abandoned, and a page a stripe
//! carried off to be written is still live in the segment of the thread that made it. mimalloc
//! only takes such a segment back when another thread goes looking, so the segments piled up, and
//! the load peaked at 7.9 GB resident with about 2.5 GB of it live.
//!
//! So the threads are kept, in the same parked pool the query drivers use, and a stage borrows as
//! many as are free. A stage that is handed fewer than it asked for runs on fewer, which is fine
//! because every caller hands its work out through a queue rather than in equal piles.

use std::sync::{Mutex, OnceLock, PoisonError};

use rudb_common::{Error, Result};
use rudb_pipeline::Pool;

/// The pool every writer in the process borrows from.
static POOL: OnceLock<Pool> = OnceLock::new();

/// The pool, made the first time a stage asks, with two threads for every core.
///
/// Two rather than one because the close runs two stages side by side, the numeric frequencies and
/// the dictionaries, and each asks for a thread a core. A pool of one a core lends all of them to
/// whichever asks first and leaves the other on the thread that called it, which made the close
/// of the 10m ClickBench load wait on every dictionary one after another. The scoped threads this
/// replaced were never capped at all, so two a core is still fewer threads than the close used to
/// start.
fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        Pool::new(std::thread::available_parallelism().map_or(1, usize::from).saturating_mul(2))
    })
}

/// Runs `worker` on up to `workers` threads, this one among them, and hands back what each returned.
///
/// Returns once every copy has finished, which is what lets `worker` borrow from the caller. The
/// first error any copy returned is the answer, and a copy that panicked is an internal error
/// naming `what`, the same as a scoped thread that panicked was.
pub(crate) fn each<T: Send>(
    workers: usize,
    what: &str,
    worker: impl Fn() -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let lease = pool().lease(workers.max(1));
    let made = Mutex::new(Vec::with_capacity(workers));
    let task = || {
        let one = worker();
        made.lock().unwrap_or_else(PoisonError::into_inner).push(one);
    };
    let ((), panicked) = lease.scatter_at_most(workers, &task, task);
    if panicked {
        return Err(Error::internal(format!("a {what} worker panicked")));
    }
    made.into_inner().unwrap_or_else(PoisonError::into_inner).into_iter().collect()
}
