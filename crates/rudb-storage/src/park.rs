//! Waiting on a row lock, `engine-v4/08-concurrency.md` section 8.4.
//!
//! A writer that finds a row held either waits for it or fails at once, and [`Wait`] decides
//! which by wait-die: a transaction that holds nothing may always wait, an older one waits for a
//! younger one, and anything else dies. Every wait edge then goes from an older transaction to a
//! younger one or from a transaction nobody can be waiting on, so no cycle can form and there is
//! no deadlock detector.
//!
//! A waiter parks on a queue in a global table hashed by the address of the lock word, the
//! `parking_lot` design, and sets `WAITERS` in the word while it holds its bucket. A release that
//! sees `WAITERS` does not free the row: it takes the bucket, writes the oldest waiter's id into
//! the word and wakes only that thread. Handoff keeps a hot row from waking a herd on every
//! commit, and handing to the oldest keeps wait-die fair.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use crate::hot::{DELTA, HELD, WAITERS};

/// Buckets in the parking table, a power of two.
const BUCKETS: usize = 64;

/// How many times a writer rereads a held row before it parks, about the 2 µs of section 8.4.
const SPINS: u32 = 64;

/// Whether and how long a writer that finds a row held may wait for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// The longest a wait lasts, `lock_timeout`. Zero never waits, the pin's behaviour.
    pub timeout: Duration,
    /// Whether the writer holds no row lock yet, which lets it wait for anyone.
    pub holds_nothing: bool,
}

impl Wait {
    /// Never wait: a held row is a conflict at once.
    pub const NEVER: Self = Self { timeout: Duration::ZERO, holds_nothing: false };

    /// Whether a writer `me` may wait for the holder `holder`. Both are ids with the top bit set,
    /// and a smaller id is an older transaction.
    #[must_use]
    pub fn allows(&self, me: u64, holder: u64) -> bool {
        !self.timeout.is_zero() && (self.holds_nothing || me < holder)
    }
}

struct Parked {
    key: usize,
    me: u64,
    granted: bool,
    thread: Thread,
}

type Queue = Mutex<Vec<Parked>>;

fn bucket(lock: &AtomicU64) -> (usize, MutexGuard<'static, Vec<Parked>>) {
    static TABLE: OnceLock<Vec<Queue>> = OnceLock::new();
    let table = TABLE.get_or_init(|| (0..BUCKETS).map(|_| Mutex::default()).collect());
    let key = std::ptr::from_ref(lock) as usize;
    let hash = (key as u64 >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - BUCKETS.ilog2());
    let queue = table[hash as usize].lock().unwrap_or_else(PoisonError::into_inner);
    (key, queue)
}

/// Waits until `lock` is free or handed to `me`, or until `deadline`. True when `me` holds the
/// row, false when the wait ran out and the row is still someone else's.
pub(crate) fn wait(lock: &AtomicU64, me: u64, deadline: Instant) -> bool {
    for _ in 0..SPINS {
        let word = lock.load(Ordering::Acquire);
        if word & HELD == 0 {
            return take(lock, me);
        }
        std::hint::spin_loop();
    }
    let (key, mut queue) = bucket(lock);
    let mut word = lock.load(Ordering::Acquire);
    loop {
        let next = if word & HELD == 0 { me | (word & (WAITERS | DELTA)) } else { word | WAITERS };
        if next == word {
            break;
        }
        match lock.compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if word & HELD == 0 => return true,
            Ok(_) => break,
            Err(now) => word = now,
        }
    }
    queue.push(Parked { key, me, granted: false, thread: thread::current() });
    drop(queue);
    loop {
        let (_, mut queue) = bucket(lock);
        let at = queue
            .iter()
            .position(|parked| parked.key == key && parked.me == me)
            .expect("a waiter stays queued until it leaves");
        if queue[at].granted {
            queue.swap_remove(at);
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            queue.swap_remove(at);
            if !queue.iter().any(|parked| parked.key == key) {
                lock.fetch_and(!WAITERS, Ordering::AcqRel);
            }
            return false;
        }
        drop(queue);
        thread::park_timeout(deadline - now);
    }
}

/// Takes a row that was free a moment ago. False if someone else took it first.
fn take(lock: &AtomicU64, me: u64) -> bool {
    let mut word = lock.load(Ordering::Acquire);
    while word & HELD == 0 {
        let held = me | (word & (WAITERS | DELTA));
        match lock.compare_exchange_weak(word, held, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => word = now,
        }
    }
    false
}

/// Lets the row behind `lock` go. When someone is parked on it, the oldest waiter gets it
/// directly and is woken, and the rest stay parked.
pub(crate) fn release(lock: &AtomicU64) {
    let mut word = lock.load(Ordering::Acquire);
    while word & WAITERS == 0 {
        match lock.compare_exchange_weak(word, word & DELTA, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(now) => word = now,
        }
    }
    // Waiters set and clear `WAITERS` only while they hold the bucket, so it stays set here.
    let (key, mut queue) = bucket(lock);
    let mut oldest: Option<usize> = None;
    let mut more = false;
    for (at, parked) in queue.iter().enumerate() {
        if parked.key != key || parked.granted {
            continue;
        }
        match oldest {
            Some(best) if queue[best].me < parked.me => more = true,
            Some(_) => {
                more = true;
                oldest = Some(at);
            }
            None => oldest = Some(at),
        }
    }
    let next = oldest.map_or(0, |at| queue[at].me | if more { WAITERS } else { 0 });
    let _ =
        lock.fetch_update(Ordering::AcqRel, Ordering::Acquire, |word| Some(next | (word & DELTA)));
    if let Some(at) = oldest {
        queue[at].granted = true;
        queue[at].thread.unpark();
    }
}

/// How many transactions are parked on `lock` and not yet handed it, for tests.
#[cfg(test)]
pub(crate) fn parked(lock: &AtomicU64) -> usize {
    let (key, queue) = bucket(lock);
    queue.iter().filter(|parked| parked.key == key && !parked.granted).count()
}
