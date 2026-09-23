//! Somewhere to run one piece of work per piece, for a layer that has no threads of its own.
//!
//! The layers that do the expensive work sit under the one that owns the thread pool. Reading the
//! parts of a stored column is in `rudb-catalog` and laying them end to end is in `rudb-vector`, and
//! neither of them may depend on `rudb-pipeline` to get a thread. So the thread comes the other way
//! round: a caller that has a lease passes in a way to run work on it, and a caller that does not
//! passes [`serially`] and gets what it always got.
//!
//! The work these are used for is the same shape both times. A piece depends on no other piece and
//! writes nothing but its own slot, so the only thing the callee needs is somewhere to say how many
//! pieces there are and a promise that all of them have finished when the call returns.

use crate::error::Result;

/// Somewhere to run one piece of work per piece, which is the thread lease of whoever is calling.
///
/// The contract is that every index below `count` is run exactly once and that all of them have
/// finished when this returns. How they are shared out is the caller's business, and the caller with
/// threads does it off a counter rather than by dealing ranges in advance, because the pieces of a
/// real column are not the same size.
///
/// It returns a `Result` because a caller with threads to lend has a thread that can panic and so
/// has something to report. The task itself returns nothing: a piece that fails says so through
/// whatever slot the callee gave it, which is also how the callee keeps its answers in piece order
/// rather than in the order the threads finished.
pub type Spread<'a> = dyn Fn(usize, &(dyn Fn(usize) + Sync)) -> Result<()> + 'a;

/// Every piece on the calling thread, in order.
///
/// # Errors
///
/// Never. The signature is [`Spread`]'s.
pub fn serially(count: usize, task: &(dyn Fn(usize) + Sync)) -> Result<()> {
    for at in 0..count {
        task(at);
    }
    Ok(())
}
