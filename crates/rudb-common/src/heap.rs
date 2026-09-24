//! A way for the program to say how freed memory goes back to the system, and for a load to ask.
//!
//! The allocator belongs to the binary and not to the library, so the library cannot call into it.
//! What it can do is say when a good moment is, and let the binary decide what that means. The shell
//! built on glibc's allocator registers `malloc_trim`, because glibc keeps what a thread frees in
//! that thread's arena and hands back only what sits at the top of it. A load allocates and frees a
//! stripe's buffers on every worker, over and over, and what it frees lands between blocks still
//! held, so the process grows long after what it holds has stopped growing. On the 10 million row
//! `hits` load on server3 the peak was 2.76 to 2.98 GB resident, of which the load accounted for
//! 1.32 GB, and 1.63 to 1.69 GB with a release after every stripe and every column closed.
//!
//! Nothing is registered by default, which makes [`release`] a load of one pointer. A library user
//! who wants the same can register their own.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// What [`release`] calls, when the program has said.
static RELEASE: OnceLock<fn()> = OnceLock::new();

/// When the process started, as far as [`release`] is concerned, so a time fits in an atomic.
static START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds after [`START`] of the last call that went through, or zero for none yet.
static LAST: AtomicU64 = AtomicU64::new(0);

/// The least time between two calls that go through.
///
/// A trim walks every arena and takes each one's lock while it does, so six workers finishing
/// stripes at once should pay for one of them and not six. Half a second is well under the few
/// seconds a worker takes to fill a stripe, so a load still trims about once a stripe.
const EVERY_MS: u64 = 500;

/// Sets what [`release`] does, once. A second call is ignored, since the allocator does not change
/// while the process runs.
pub fn on_release(release: fn()) {
    let _ = RELEASE.set(release);
}

/// Asks the allocator to give back what it is holding free, if the program said how and nobody
/// else asked in the last half second.
///
/// For a place that has just dropped a lot, like a load that has written a stripe. It is a hint and
/// never fails, and the caller does not wait for anybody else's call to finish.
pub fn release() {
    let Some(release) = RELEASE.get() else { return };
    let start = START.get_or_init(Instant::now);
    let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX).max(1);
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < EVERY_MS {
        return;
    }
    if LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn count() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn calls_close_together_release_once() {
        on_release(count);
        release();
        release();
        release();
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }
}
