//! The page sized runs a scan keeps instead of handing them back to the allocator.
//!
//! A reader of one column chunk holds two buffers that are megabytes on a real file: the compressed
//! bytes it reads a page into, and the run it decompresses that page into. Both are recycled from
//! page to page already, and neither survives the chunk, because a cursor is built per column per
//! row group and dropped at the end of it. On `hits` that is a four and a half megabyte run and a
//! ten and a half megabyte one, taken from the allocator and given back nine times for one scan of
//! one column.
//!
//! A string column is worse than that. Its page becomes the column's arena, because the alternative
//! is copying every byte of it, so the run leaves with the vector and the reader is handed an empty
//! buffer. It is not recycled even from page to page.
//!
//! # What giving a run back actually costs
//!
//! Two things, and the second is the larger one.
//!
//! A run that comes back empty has to be grown before it can be written into, and growing a `Vec`
//! writes zeros over every byte. Ten and a half megabytes of zeros nine times is ninety four
//! megabytes of memory bandwidth spent on bytes the decompressor is about to overwrite. A run that
//! comes back at the size it was last time is grown by nothing and zeroed not at all.
//!
//! Then there is what the allocator does with a block that size. glibc serves it with `mmap` when
//! its threshold is low and returns it to the kernel on free, so the next one faults in a four
//! kilobyte page at a time before a byte is written. Its threshold is not fixed: freeing an mmapped
//! block raises it, so a program that frees one large block teaches the allocator to keep the next
//! one. That is an accident, it depends on the order of a program's allocations, and it is worth
//! not depending on. Measured on a scan of `URL` in `hits-1m-snappy.parquet` on one thread, running
//! with `MALLOC_MMAP_THRESHOLD_` and `MALLOC_TRIM_THRESHOLD_` both raised so that nothing is ever
//! returned took the query from 109.03 milliseconds to 100.56. Neither setting does anything on its
//! own, because setting either one turns off glibc's adjustment of the other, and each alone
//! measured slower than the default.
//!
//! # Why a ring and not a field
//!
//! The ends of this are [`share`], called from `values::place` when a page becomes an arena,
//! [`park`], called when a cursor is dropped, and [`take`], called when the next page needs
//! somewhere to go. There is no object all three can see. A cursor is built per column chunk and
//! the string decoder is a free function several calls below the scan, so connecting them by a
//! parameter means a pool argument on every decoder in `values.rs` for the benefit of one of them.
//!
//! A thread local also happens to be the right place rather than only the convenient one. A run
//! handed back to the thread that faulted it in is the one whose pages are already in that core's
//! translation buffers, and a scan reads a column chunk on one thread from start to end.
//!
//! # What it costs when nobody collects
//!
//! A parked run is held until another page wants it, and if the scan ends first it is held until
//! the thread does. That is bounded by [`BUDGET`] bytes on each thread that has read a page large
//! enough to be worth parking, which is also what stops one enormous run from being kept on the off
//! chance: a run that does not fit in the budget on its own is never parked at all.

use std::cell::RefCell;
use std::sync::Arc;

/// Runs smaller than this are left to the allocator, which is better at them than this is.
///
/// glibc serves anything under its mmap threshold out of a free list it already keeps, so parking a
/// small run buys nothing and costs budget that a page sized run wanted. The threshold moves at run
/// time, so this is the floor of where it can be rather than where it is.
const FLOOR: usize = 128 << 10;

/// How many bytes of parked runs one thread holds.
///
/// A scan of `hits` wants fifteen of these megabytes for one string column, so this is room for a
/// couple of them, and a query reading more string columns than that gets the behaviour it had
/// before this existed, which is an allocation per page. Bounded rather than generous on purpose: a
/// run parked here is memory a query is not using and cannot be asked to give back, and thirty two
/// megabytes a thread is already the same order as the chunks a scan has in flight.
const BUDGET: usize = 32 << 20;

thread_local! {
    /// The runs this thread is holding a handle to.
    ///
    /// An entry whose strong count is one is free and can be taken back. An entry whose count is
    /// higher is a page that a vector somewhere downstream is still reading, and it is here because
    /// it will become free later, which is the whole mechanism: nothing has to be told when a chunk
    /// dies. Oldest first, because that is the order to give up on.
    static PARKED: RefCell<Vec<Arc<Vec<u8>>>> = const { RefCell::new(Vec::new()) };
}

/// Hands `page` out as an arena and keeps a handle so the run can come back.
///
/// The caller gets a handle to put in a [`Buffer`](rudb_vector::Buffer), and when every vector over
/// that page has been dropped the handle kept here is the only one left and [`take`] can empty it.
pub(crate) fn share(page: Vec<u8>) -> Arc<Vec<u8>> {
    let page = Arc::new(page);
    keep(&page);
    page
}

/// Parks a run whose owner is done with it, for the next page that wants one.
///
/// What [`share`] does for a run that leaves with a vector, done for one that comes back by value.
/// An empty run is not parked, so a caller that has nothing to give does not have to check.
pub(crate) fn park(run: Vec<u8>) {
    if !run.is_empty() {
        keep(&Arc::new(run));
    }
}

/// Puts a handle on the ring if the run behind it is worth keeping and there is room.
fn keep(page: &Arc<Vec<u8>>) {
    let size = page.capacity();
    if !(FLOOR..=BUDGET).contains(&size) {
        return;
    }
    PARKED.with_borrow_mut(|parked| {
        let mut held: usize = parked.iter().map(|run| run.capacity()).sum();
        while held + size > BUDGET && !parked.is_empty() {
            // The oldest, which has had the longest to be wanted again and was not.
            held -= parked.remove(0).capacity();
        }
        parked.push(Arc::clone(page));
    });
}

/// A run of at least `want` bytes to write into, from this thread's ring if one is free.
///
/// Empty when nothing is free, which is what the first page of a scan sees and what every page sees
/// on a query that holds on to its chunks. The caller cannot tell the difference and does not need
/// to: either way it is a `Vec<u8>` to grow into.
#[must_use]
pub(crate) fn take(want: usize) -> Vec<u8> {
    PARKED
        .with_borrow_mut(|parked| {
            let mut best: Option<usize> = None;
            for (at, run) in parked.iter().enumerate() {
                if Arc::strong_count(run) != 1 {
                    continue;
                }
                // The smallest run that is large enough, and the largest run when none is. A scan
                // holding a ten megabyte arena and a four megabyte read buffer wants them to go back to
                // the two callers they came from rather than the read buffer taking the arena's run and
                // the arena then having to grow one.
                let better = best.is_none_or(|old| {
                    let (new, old) = (run.capacity(), parked[old].capacity());
                    if (new >= want) == (old >= want) {
                        if new >= want { new < old } else { new > old }
                    } else {
                        new >= want
                    }
                });
                if better {
                    best = Some(at);
                }
            }
            let mut run = parked.swap_remove(best?);
            Some(std::mem::take(Arc::get_mut(&mut run)?))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{BUDGET, FLOOR, PARKED, park, share, take};

    /// Each test gets a thread of its own, because the ring is per thread and the harness runs
    /// tests on shared ones. Cheaper than a lock and it is also how the reader uses this.
    fn alone(test: fn()) {
        std::thread::spawn(test).join().expect("the test thread");
    }

    #[test]
    fn a_shared_run_comes_back_once_the_last_reader_of_it_is_gone() {
        alone(|| {
            let page = share(vec![7u8; FLOOR]);
            let address = page.as_ptr();
            assert!(take(FLOOR).is_empty(), "a run still being read was handed out");
            drop(page);
            let back = take(FLOOR);
            assert_eq!(back.as_ptr(), address, "the run was reallocated rather than reused");
            assert_eq!(back.len(), FLOOR);
            assert!(take(FLOOR).is_empty(), "the same run was handed out twice");
        });
    }

    #[test]
    fn a_parked_run_comes_back_at_the_length_it_was_parked_at() {
        alone(|| {
            let mut run = vec![0u8; FLOOR];
            let address = run.as_ptr();
            park(std::mem::take(&mut run));
            let back = take(FLOOR);
            assert_eq!(back.as_ptr(), address);
            // The length is the point. A run that comes back empty has to be grown before it can be
            // written into, and growing a vector writes zeros over every byte of it.
            assert_eq!(back.len(), FLOOR);
        });
    }

    #[test]
    fn a_run_too_small_or_too_large_to_be_worth_keeping_is_not_kept() {
        alone(|| {
            park(vec![0u8; FLOOR - 1]);
            assert!(take(FLOOR - 1).is_empty(), "a run under the floor was parked");
            park(vec![0u8; BUDGET + 1]);
            assert!(take(BUDGET).is_empty(), "a run over the budget was parked");
            park(Vec::new());
            assert!(take(0).is_empty(), "an empty run was parked");
        });
    }

    #[test]
    fn the_ring_holds_the_bytes_it_says_it_holds_and_no_more() {
        alone(|| {
            let held: Vec<_> = (0..8).map(|_| share(vec![0u8; BUDGET / 4])).collect();
            PARKED.with_borrow(|parked| {
                let bytes: usize = parked.iter().map(|run| run.capacity()).sum();
                assert!(bytes <= BUDGET, "{bytes} bytes parked against a budget of {BUDGET}");
            });
            drop(held);
        });
    }

    /// The reader has two callers of different sizes and they should each get their own run back.
    #[test]
    fn the_smallest_run_that_is_large_enough_is_the_one_handed_out() {
        alone(|| {
            park(vec![0u8; FLOOR * 4]);
            park(vec![0u8; FLOOR]);
            park(vec![0u8; FLOOR * 2]);
            assert_eq!(take(FLOOR * 2).len(), FLOOR * 2);
            assert_eq!(take(FLOOR).len(), FLOOR);
            // Nothing left is large enough, so the largest is better than growing from nothing.
            assert_eq!(take(FLOOR * 8).len(), FLOOR * 4);
        });
    }
}
