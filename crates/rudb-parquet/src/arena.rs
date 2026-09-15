//! The page sized runs a scan keeps instead of handing them back to the allocator.
//!
//! A column of integers decompresses into a buffer the walker owns and gets back when the values
//! have been copied out, which is `Cursor::spare` in `reader.rs` and costs one allocation for a
//! whole file. A column of strings does not work like that. The page becomes the column's arena,
//! because the alternative is copying every byte of it, so it leaves with the vector and the walker
//! is handed an empty buffer. On `hits` that is ten and a half megabytes a row group, taken from
//! the allocator and given back nine times for one scan of one column.
//!
//! Giving it back is the expensive half. glibc serves a block that size with `mmap` and returns it
//! to the kernel on free, so the next page faults in every one of its twenty five hundred pages on
//! the first write. Measured on the `URL` column of `hits-1m-snappy.parquet` on one thread, forcing
//! glibc to keep everything with `MALLOC_MMAP_THRESHOLD_` and `MALLOC_TRIM_THRESHOLD_` both raised
//! took the query from 113.08 milliseconds to 104.08. The decompress stage went 57.35 to 52.37 and
//! the decode stage 32.88 to 30.03, and it is the second of those that says what this is: decode
//! does not allocate, it writes the views, so the only thing it can have been paying is faults on
//! the arena it was handed.
//!
//! Neither of those two glibc settings does anything on its own, which is worth knowing before
//! anybody tries the one line version of this. Setting either one disables glibc's dynamic
//! adjustment of the other, and each alone measured slower than the default: the trim threshold
//! alone 128.95 milliseconds, the mmap threshold alone 126.57.
//!
//! # Why a ring and not a field
//!
//! The two ends of this are [`share`], called from `values::place` when a page becomes an arena,
//! and [`take`], called from `chunk::decompressed` when the next page needs somewhere to go. There
//! is no object that both of those can see. The page walker is built per column chunk and the
//! decoder is a free function several calls below the scan, so connecting them by a parameter means
//! a pool argument on every decoder in `values.rs` for the benefit of one of them.
//!
//! A thread local also happens to be the right place rather than only the convenient one. A run
//! that is handed back to the thread that faulted it in is the one whose pages are already in that
//! core's translation buffers, and a scan that reads a string column reads it on the same thread
//! for the whole of a row group.
//!
//! # What it costs when nobody collects
//!
//! A parked run is held until another page wants it, and if the scan ends first it is held until
//! the thread does. That is bounded by [`KEEP`] runs on each thread that has decoded a string page,
//! and by [`CEILING`] on how large a run is worth keeping at all, because a page that big is rare
//! enough that the next one is unlikely to want it and large enough that sitting on it is worse
//! than faulting it.

use std::cell::RefCell;
use std::sync::Arc;

/// How many runs one thread parks.
///
/// Two rather than one because a scan reading two string columns alternates between them, and two
/// rather than more because the runs are page sized and a thread holding four of them is holding
/// forty megabytes for a scan that may already be over. A third column thrashes this and gets the
/// behaviour it has today, which is an allocation per page.
const KEEP: usize = 2;

/// Runs smaller than this are left to the allocator, which is better at them than this is.
///
/// glibc serves anything under its mmap threshold from a free list it already keeps, so parking a
/// small run buys nothing and costs a slot that a page sized run wanted. The threshold moves at run
/// time, so this is the floor of where it can be rather than where it is.
const FLOOR: usize = 128 << 10;

/// Runs larger than this are given back rather than parked.
///
/// A page this size is not the steady state of anything, so the next page almost certainly does not
/// want a run this large, and holding it to find that out is the memory a whole scan was supposed
/// to fit in.
const CEILING: usize = 64 << 20;

thread_local! {
    /// The runs this thread is holding a second handle to.
    ///
    /// An entry whose strong count is one is free and can be taken back. An entry whose count is
    /// higher is still being read by a vector somewhere downstream and is here because it will
    /// become free later, which is the whole mechanism: nothing has to be told when a chunk dies.
    static PARKED: RefCell<Vec<Arc<Vec<u8>>>> = const { RefCell::new(Vec::new()) };
}

/// Hands `page` out as an arena and keeps a handle so the run can come back.
///
/// The caller gets a handle to put in a [`Buffer`](rudb_vector::Buffer), and when every vector over
/// that page has been dropped the handle kept here is the only one left and [`take`] can empty it.
pub(crate) fn share(page: Vec<u8>) -> Arc<Vec<u8>> {
    let page = Arc::new(page);
    if (FLOOR..=CEILING).contains(&page.capacity()) {
        PARKED.with_borrow_mut(|parked| {
            if parked.len() >= KEEP {
                // The oldest, which is the one that has had the longest to become free and has not,
                // so it is the entry least likely to be worth waiting on.
                parked.remove(0);
            }
            parked.push(Arc::clone(&page));
        });
    }
    page
}

/// A run to decompress into, reused from this thread's ring if one is free.
///
/// Empty when nothing is free, which is what the first page of a scan sees and what every page sees
/// on a query that holds its chunks. The caller cannot tell the difference and does not need to:
/// either way it is a `Vec<u8>` to grow into.
#[must_use]
pub(crate) fn take() -> Vec<u8> {
    PARKED
        .with_borrow_mut(|parked| {
            let mut best: Option<usize> = None;
            for (at, run) in parked.iter().enumerate() {
                // Largest free run rather than first, so that a small one parked earlier does not send
                // a page sized decompression into a grow and a copy while a page sized run sits here.
                let free = Arc::strong_count(run) == 1;
                if free && best.is_none_or(|old| run.capacity() > parked[old].capacity()) {
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
    use super::{CEILING, FLOOR, KEEP, PARKED, share, take};

    /// Each test gets a thread of its own, because the ring is per thread and the harness runs
    /// tests on shared ones. Cheaper than a lock and it is also how the reader uses this.
    fn alone(test: fn()) {
        std::thread::spawn(test).join().expect("the test thread");
    }

    #[test]
    fn a_run_comes_back_once_the_last_reader_of_it_is_gone() {
        alone(|| {
            let page = share(vec![7u8; FLOOR]);
            let address = page.as_ptr();
            assert!(take().is_empty(), "a run still being read was handed out");
            drop(page);
            let back = take();
            assert_eq!(back.as_ptr(), address, "the run was reallocated rather than reused");
            assert_eq!(back.len(), FLOOR);
            assert!(take().is_empty(), "the same run was handed out twice");
        });
    }

    #[test]
    fn a_run_too_small_or_too_large_to_be_worth_keeping_is_not_kept() {
        alone(|| {
            drop(share(vec![0u8; FLOOR - 1]));
            assert!(take().is_empty(), "a run under the floor was parked");
            drop(share(Vec::with_capacity(CEILING + 1)));
            assert!(take().is_empty(), "a run over the ceiling was parked");
        });
    }

    #[test]
    fn the_ring_holds_the_runs_it_says_it_holds_and_no_more() {
        alone(|| {
            let held: Vec<_> = (0..KEEP + 2).map(|_| share(vec![0u8; FLOOR])).collect();
            PARKED.with_borrow(|parked| assert_eq!(parked.len(), KEEP));
            drop(held);
            for _ in 0..KEEP {
                assert!(!take().is_empty(), "a parked run was not handed back");
            }
            assert!(take().is_empty());
        });
    }

    #[test]
    fn the_largest_free_run_is_the_one_handed_out() {
        alone(|| {
            drop(share(vec![0u8; FLOOR]));
            drop(share(vec![0u8; FLOOR * 3]));
            assert_eq!(take().len(), FLOOR * 3);
            assert_eq!(take().len(), FLOOR);
        });
    }
}
