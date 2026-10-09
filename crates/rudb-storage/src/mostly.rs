//! A lock for what nearly every statement reads and few write, `engine-v4/13-the-point-path.md`
//! section 13.5.
//!
//! A reader of `std::sync::RwLock` counts itself in the lock's one word, so every statement on
//! every core writes the same cache line twice, and at a few dozen threads the line moving between
//! cores costs more than a point read does. Here a reader counts itself in a slot of its own, on
//! a line no other thread uses unless more threads run than there are slots, and then reads one
//! flag that only a writer writes. A writer takes the writers' mutex, raises the flag and waits for
//! every slot to empty. The reader's count and the writer's flag are both sequentially consistent,
//! so of a reader and a writer arriving together at least one sees the other: the reader backs out
//! and waits on the mutex, or the writer waits for the reader to leave.
//!
//! Writing costs a pass over the slots, which is a few hundred nanoseconds next to a write that
//! commits. A writer waiting for a long query spins briefly, then yields, then sleeps in growing
//! steps, because a reader leaving wakes nobody. A reader that finds a writer parks on the mutex
//! the writer holds and so wakes when it lets go.
//!
//! As with `RwLock`, a thread that takes the lock again while it holds it can deadlock against a
//! waiting writer, and nothing is poisoned: a writer that panics leaves what it wrote, which is
//! what the callers did with a poisoned `RwLock` anyway.

use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Reader slots per lock, a power of two. Threads are given slots in the order they first read,
/// so two threads share one only past this many.
const SLOTS: usize = 64;

/// One reader count, on two cache lines of its own so the adjacent line prefetch of one core does
/// not pull in another's.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Slot(AtomicUsize);

/// A value read under per-thread counts and written under a mutex, see the module.
pub struct ReadMostly<T> {
    slots: Box<[Slot]>,
    /// Raised by a writer once it holds `writers`, and lowered as it lets go.
    writing: AtomicBool,
    writers: Mutex<()>,
    value: UnsafeCell<T>,
}

// SAFETY: the lock owns its value, so moving the lock to another thread moves the value, which
// asks no more of `T` than `Send`.
unsafe impl<T: Send> Send for ReadMostly<T> {}

// SAFETY: readers on several threads share `&T`, which needs `Sync`, and a writer on any thread
// gets `&mut T`, which needs `Send`. A writer runs only once every slot is empty and the flag stops
// a reader from entering, see `read` and `write`, so a `&mut T` never exists beside a `&T`.
unsafe impl<T: Send + Sync> Sync for ReadMostly<T> {}

impl<T> fmt::Debug for ReadMostly<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the value, which a thread printing this while it writes would wait on forever.
        f.debug_struct("ReadMostly").finish_non_exhaustive()
    }
}

impl<T: Default> Default for ReadMostly<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// The slot of the calling thread.
fn mine() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static MINE: usize = NEXT.fetch_add(1, Ordering::Relaxed) % SLOTS;
    }
    MINE.with(|mine| *mine)
}

impl<T> ReadMostly<T> {
    /// A lock holding `value`.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::with_slots(value, SLOTS)
    }

    /// A lock holding `value` with one reader slot, for a value that one thread reads nearly
    /// always. It takes 128 bytes, where [`ReadMostly::new`] takes 8 KiB.
    #[must_use]
    pub fn narrow(value: T) -> Self {
        Self::with_slots(value, 1)
    }

    fn with_slots(value: T, slots: usize) -> Self {
        debug_assert!(slots.is_power_of_two() && slots <= SLOTS);
        Self {
            slots: (0..slots).map(|_| Slot::default()).collect(),
            writing: AtomicBool::new(false),
            writers: Mutex::new(()),
            value: UnsafeCell::new(value),
        }
    }

    /// Reads the value, waiting for a writer that holds it.
    pub fn read(&self) -> ReadGuard<'_, T> {
        // The number of slots is a power of two, so the mask keeps the index in the slots.
        let slot = &self.slots[mine() & (self.slots.len() - 1)];
        loop {
            slot.0.fetch_add(1, Ordering::SeqCst);
            if !self.writing.load(Ordering::SeqCst) {
                return ReadGuard { lock: self, slot };
            }
            slot.0.fetch_sub(1, Ordering::SeqCst);
            // The writer raised the flag holding the mutex, so this waits for it to let go.
            drop(self.writers.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }

    /// Writes the value, waiting for every reader and writer that holds it.
    pub fn write(&self) -> WriteGuard<'_, T> {
        let held = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        self.writing.store(true, Ordering::SeqCst);
        for slot in &*self.slots {
            let mut waited = 0;
            while slot.0.load(Ordering::SeqCst) != 0 {
                pause(&mut waited);
            }
        }
        WriteGuard { lock: self, _held: held }
    }

    /// The value, through a lock nobody else can be holding.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// The value, with the lock gone.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

/// One wait of a writer for a reader to leave: a spin, then a yield, then a sleep that doubles up
/// to a millisecond.
fn pause(waited: &mut u32) {
    *waited += 1;
    match *waited {
        0..=64 => std::hint::spin_loop(),
        65..=128 => std::thread::yield_now(),
        steps => {
            let micros = 10_u64 << (steps - 129).min(7);
            std::thread::sleep(Duration::from_micros(micros.min(1_000)));
        }
    }
}

/// A read of a [`ReadMostly`], which lets go when dropped.
pub struct ReadGuard<'a, T> {
    lock: &'a ReadMostly<T>,
    slot: &'a Slot,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard's count is in a slot, and a writer makes its `&mut T` only once it has
        // seen every slot empty after raising the flag, and the flag stays up until that reference
        // is gone. A reader counted in before the flag went up keeps the writer waiting; one that
        // came after saw the flag and backed out. So no `&mut T` exists while this guard does.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        self.slot.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<T: fmt::Debug> fmt::Debug for ReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// A write of a [`ReadMostly`], which lets go when dropped.
pub struct WriteGuard<'a, T> {
    lock: &'a ReadMostly<T>,
    _held: MutexGuard<'a, ()>,
}

impl<'a, T> WriteGuard<'a, T> {
    /// Turns the write into a read with no writer between them, for a statement that changes the
    /// value only before it runs a long read. The reader is counted in while the flag is still up
    /// and the mutex still held, so a writer waiting on the mutex then waits for this reader too.
    pub fn downgrade(this: Self) -> ReadGuard<'a, T> {
        let lock = this.lock;
        let slot = &lock.slots[mine() & (lock.slots.len() - 1)];
        slot.0.fetch_add(1, Ordering::SeqCst);
        drop(this);
        ReadGuard { lock, slot }
    }
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as for `deref_mut`, of which this is a shorter borrow.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: this guard holds the writers' mutex, so no other writer exists, and it raised the
        // flag and saw every slot empty before it was made, so no reader holds a `&T` and none can
        // come in until it drops and lowers the flag.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        // Before the mutex, which is the field dropped after this, so a reader parked on the mutex
        // finds the flag down when it wakes.
        self.lock.writing.store(false, Ordering::SeqCst);
    }
}

impl<T: fmt::Debug> fmt::Debug for WriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::ReadMostly;

    #[test]
    fn readers_see_whole_writes_and_writers_wait_for_readers() {
        readers_and_writers(ReadMostly::new((0, 0)));
    }

    #[test]
    fn readers_that_share_one_slot_see_whole_writes() {
        readers_and_writers(ReadMostly::narrow((0, 0)));
    }

    fn readers_and_writers(lock: ReadMostly<(u64, u64)>) {
        let lock = Arc::new(lock);
        let done = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..6)
            .map(|_| {
                let lock = Arc::clone(&lock);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    let mut seen = 0;
                    while !done.load(Ordering::Relaxed) {
                        let held = lock.read();
                        assert_eq!(held.0, held.1, "a reader saw half a write");
                        assert!(held.0 >= seen, "a reader went back in time");
                        seen = held.0;
                    }
                    seen
                })
            })
            .collect();
        let writers: Vec<_> = (0..3)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..2_000 {
                        let mut held = lock.write();
                        held.0 += 1;
                        std::hint::spin_loop();
                        held.1 += 1;
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer finished");
        }
        done.store(true, Ordering::Relaxed);
        for reader in readers {
            assert!(reader.join().expect("a reader finished") <= 6_000);
        }
        assert_eq!(*lock.read(), (6_000, 6_000));
    }

    #[test]
    fn a_reader_waits_for_a_writer() {
        let lock = Arc::new(ReadMostly::new(0));
        let held = lock.write();
        let reader = {
            let lock = Arc::clone(&lock);
            std::thread::spawn(move || *lock.read())
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut held = held;
        *held = 7;
        drop(held);
        assert_eq!(reader.join().expect("the reader finished"), 7);
        let mut lock = Arc::try_unwrap(lock).expect("the only handle");
        *lock.get_mut() += 1;
        assert_eq!(lock.into_inner(), 8);
    }

    #[test]
    fn a_downgraded_write_lets_readers_in_and_keeps_writers_out() {
        let lock = Arc::new(ReadMostly::new(0));
        let mut held = lock.write();
        *held = 1;
        let read = super::WriteGuard::downgrade(held);
        // Another reader comes in beside the downgraded one.
        let reader = {
            let lock = Arc::clone(&lock);
            std::thread::spawn(move || *lock.read())
        };
        assert_eq!(reader.join().expect("the reader finished"), 1);
        // A writer waits until the downgraded read is gone.
        let wrote = Arc::new(AtomicBool::new(false));
        let writer = {
            let lock = Arc::clone(&lock);
            let wrote = Arc::clone(&wrote);
            std::thread::spawn(move || {
                *lock.write() = 2;
                wrote.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!wrote.load(Ordering::SeqCst), "a writer came in beside a read");
        assert_eq!(*read, 1);
        drop(read);
        writer.join().expect("the writer finished");
        assert_eq!(*lock.read(), 2);
    }
}
