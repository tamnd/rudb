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
//! Whatever glibc decides to do with a block that size, which is not a fixed thing. It serves one
//! with `mmap` when its threshold is low and returns it to the kernel on free, so the next one
//! faults in a four kilobyte page at a time before a byte is written. The threshold moves: freeing
//! an mmapped block raises it, so a program that frees one large block teaches the allocator to
//! keep the next one, and where a scan lands in that depends on the order of its allocations.
//!
//! Measured on a scan of `URL` in `hits-1m-snappy.parquet` on one thread, running with
//! `MALLOC_MMAP_THRESHOLD_` and `MALLOC_TRIM_THRESHOLD_` both raised so that nothing is ever
//! returned took the query from 109.03 milliseconds to 100.56. Neither setting does anything on its
//! own, because setting either one turns off glibc's adjustment of the other, and each alone
//! measured slower than the default. Eight of those milliseconds are what a scan is paying for
//! handing back runs it is about to ask for again, and a run that never goes back is a run none of
//! that can happen to.
//!
//! A run that comes back also comes back at the length it was, so the decompressor writes into it
//! rather than growing it first. That is worth less than it sounds on this path: `snappy` grows an
//! empty buffer with `vec![0u8; expected]`, which is `alloc_zeroed` and therefore pages the kernel
//! has not had to write to yet. It is the uncompressed and ZSTD paths, which `resize` instead, that
//! pay for the zeros.
//!
//! # Why a thread local and not a field
//!
//! The two ends of this are [`share`], called from `values::place` when a page becomes an arena,
//! and [`take`], called when the next page needs somewhere to go. There is no object both can see.
//! A cursor is built per column chunk and the string decoder is a free function several calls below
//! the scan, so connecting them by a parameter means a pool argument on every decoder in
//! `values.rs` for the benefit of one of them.
//!
//! A thread local also happens to be the right place rather than only the convenient one. A run
//! handed back to the thread that faulted it in is the one whose pages are already in that core's
//! translation buffers, and a scan reads a column chunk on one thread from start to end.
//!
//! # What it costs when nobody collects
//!
//! One run per thread, and only while that run is free. The slot holds the last page a string
//! column was built over, and the next one replaces it whether or not it was ever reused, so a
//! scan cannot accumulate them. A run still being read downstream costs nothing to have a handle
//! to, because the vector reading it is holding it up anyway.
//!
//! One slot is what stops this from making a short scan worse, and that is not a guess. A version
//! of this with a thirty two megabyte budget and every cursor parking its buffers on the way out
//! measured query 37 of ClickBench at 43.98 milliseconds against 42.14, with peak resident memory
//! up from 35.6 to 51.1 megabytes. Query 37 reads two row groups after pruning, so the arena of the
//! first was still being read when the second was decompressed, and the ring ended up holding the
//! first while the second was allocated beside it. Replacing the slot rather than growing a pool
//! means the worst case is the behaviour without it.
//!
//! It also means two string columns in one scan take turns evicting each other and neither is
//! reused. That is the behaviour this had before it existed, so it is a missed win rather than a
//! cost, and a slot per column is a change to make when a query that wants it has been measured.

use std::cell::RefCell;
use std::sync::Arc;

/// Runs smaller than this are left to the allocator, which is better at them than this is.
///
/// glibc serves anything under its mmap threshold out of a free list it already keeps, so holding a
/// small run buys nothing and costs the slot that a page sized run wanted. The threshold moves at
/// run time, so this is the floor of where it can be rather than where it is.
const FLOOR: usize = 128 << 10;

thread_local! {
    /// The last page this thread built a string column over.
    ///
    /// Free and reusable when its strong count is one. A higher count is a page a vector somewhere
    /// downstream is still reading, and the handle is here because it will become free later, which
    /// is the whole mechanism: nothing has to be told when a chunk dies.
    static PARKED: RefCell<Option<Arc<Vec<u8>>>> = const { RefCell::new(None) };
}

/// Hands `page` out as an arena and keeps a handle so the run can come back.
///
/// The caller gets a handle to put in a [`Buffer`](rudb_vector::Buffer), and when every vector over
/// that page has been dropped the handle kept here is the only one left and [`take`] can empty it.
///
/// Whatever was in the slot is let go of here, reused or not. A page that has not come free by the
/// time the next one is built is a page this is not going to get, and holding it while the next one
/// is allocated beside it is the one way this could cost more memory than it saves.
pub(crate) fn share(page: Vec<u8>) -> Arc<Vec<u8>> {
    let page = Arc::new(page);
    if page.capacity() >= FLOOR {
        PARKED.with_borrow_mut(|parked| *parked = Some(Arc::clone(&page)));
    }
    page
}

/// The last page, if nothing is reading it any more.
///
/// Empty when the slot is empty or its page is still being read, which is what the first page of a
/// scan sees and what every page sees on a query that holds on to its chunks. The caller cannot
/// tell the difference and does not need to: either way it is a `Vec<u8>` to grow into.
///
/// It comes back at the length it was, not empty, which is the point. A run that comes back empty
/// has to be grown before it can be written into, and growing one is an allocation and a walk over
/// every byte.
#[must_use]
pub(crate) fn take() -> Vec<u8> {
    PARKED
        .with_borrow_mut(|parked| {
            let mut page = parked.take()?;
            match Arc::get_mut(&mut page) {
                Some(run) => Some(std::mem::take(run)),
                None => {
                    // Still being read, so put the handle back and wait for the next page.
                    *parked = Some(page);
                    None
                }
            }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{FLOOR, PARKED, share, take};

    /// Each test gets a thread of its own, because the slot is per thread and the harness runs
    /// tests on shared ones. Cheaper than a lock and it is also how the reader uses this.
    fn alone(test: fn()) {
        std::thread::spawn(test).join().expect("the test thread");
    }

    #[test]
    fn a_page_comes_back_once_the_last_reader_of_it_is_gone() {
        alone(|| {
            let page = share(vec![7u8; FLOOR]);
            let address = page.as_ptr();
            assert!(take().is_empty(), "a page still being read was handed out");
            drop(page);
            let back = take();
            assert_eq!(back.as_ptr(), address, "the page was reallocated rather than reused");
            // The length is the point. A run that comes back empty has to be grown first.
            assert_eq!(back.len(), FLOOR);
            assert!(take().is_empty(), "the same run was handed out twice");
        });
    }

    #[test]
    fn a_page_still_being_read_stays_in_the_slot_rather_than_being_thrown_away() {
        alone(|| {
            let page = share(vec![0u8; FLOOR]);
            assert!(take().is_empty());
            assert!(PARKED.with_borrow(Option::is_some), "the handle was dropped on a failed take");
            drop(page);
            assert!(!take().is_empty(), "the page did not come back after its reader went");
        });
    }

    #[test]
    fn a_page_too_small_to_be_worth_keeping_is_not_kept() {
        alone(|| {
            drop(share(vec![0u8; FLOOR - 1]));
            assert!(take().is_empty(), "a run under the floor was kept");
        });
    }

    /// The property that stops this from ever holding more than one spare run. Whatever is in the
    /// slot is let go of when the next page arrives, reused or not.
    #[test]
    fn the_slot_holds_one_page_and_the_next_one_replaces_it() {
        alone(|| {
            let first = share(vec![1u8; FLOOR]);
            let second = share(vec![2u8; FLOOR * 2]);
            assert_eq!(Arc::strong_count(&first), 1, "the first page is still held somewhere");
            drop(second);
            assert_eq!(take().len(), FLOOR * 2);
        });
    }
}
