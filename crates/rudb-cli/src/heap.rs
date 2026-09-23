//! The allocator the shell installs, and the one question it asks differently.
//!
//! mimalloc is the allocator and `main.rs` says why it is the one. What is here is the four call
//! trait implementation that connects Rust's allocation to mimalloc's. The `mimalloc` crate ships
//! one of those too, and this replaces it for one reason.
//!
//! That crate answers every allocation with `mi_malloc_aligned(size, align)` whatever the alignment
//! is. Almost every allocation the engine makes wants 8 or 16, which mimalloc gives out without
//! being asked, and asking anyway is not free. `mi_malloc_aligned` has a fast path for a block
//! small enough to come off a thread's free list of small blocks, which is a kilobyte, and a column
//! of a chunk is two thousand values and never is. So every buffer a query takes went down a path
//! marked `noinline` that checks the alignment is a power of two, checks the size against the small
//! limit, works out that a plain allocation would have been aligned enough after all, makes one,
//! and checks the answer.
//!
//! Every one of those checks is over a `Layout` the caller knew at compile time. So the question is
//! asked here, at the call, where the compiler folds it away, and an allocation that wants no more
//! alignment than mimalloc gives anyway calls `mi_malloc`. The rest call `mi_malloc_aligned` as
//! before.
//!
//! Growing is the same story with a different cutoff. `mi_realloc_aligned` passes an alignment of
//! eight or less straight through and takes the slow path at sixteen, and the slow path always
//! allocates and copies rather than growing a block where it is. Sixteen is the alignment of an
//! `i128`, so a growing `Vec` of decimals or of an aggregate's accumulators copied itself every
//! time.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::{c_int, c_long};

use libmimalloc_sys::{
    mi_free, mi_malloc, mi_malloc_aligned, mi_realloc, mi_realloc_aligned, mi_zalloc,
    mi_zalloc_aligned,
};

/// `mi_option_purge_delay` in mimalloc 2's `mimalloc.h`, the sixteenth entry of `mi_option_e`.
const PURGE_DELAY: c_int = 15;

// The two calls of mimalloc's option interface this needs. `libmimalloc-sys` declares them only
// behind a feature that also pulls in a dependency, and they are in the library it links either
// way, so they are declared here as they are in the header.
#[allow(unsafe_code)]
unsafe extern "C" {
    fn mi_option_set(option: c_int, value: c_long);
    #[cfg(test)]
    fn mi_option_get(option: c_int) -> c_long;
}

/// Tells mimalloc to keep the memory a query frees rather than hand it back to the system.
///
/// By default mimalloc returns freed memory ten milliseconds after it is freed. A load frees and
/// takes gigabytes over and over inside one statement: the sorted SF1 `lineitem` CTAS peaks at
/// 2.3GB and mimalloc's own statistics show it purging 4.7GiB along the way, every byte of which
/// is faulted back in when it is taken again. With the purge off that is 0, and over 21
/// interleaved runs the median wall time was about 7 percent lower and the system time a seventh
/// lower. A delay of a second did no better than the default, so it is off rather
/// than delayed. The price is what the process holds after the load, 2248MB against 2099MB there,
/// and the next statement takes from that before it asks the system for more. Setting
/// `MIMALLOC_PURGE_DELAY` in the environment still decides, for a user who wants it back.
pub(crate) fn keep_freed_memory() {
    if std::env::var_os("MIMALLOC_PURGE_DELAY").is_some() {
        return;
    }
    // SAFETY: an option is an integer mimalloc reads when it next decides whether to purge, and
    // setting one is allowed at any time, including after the first allocation.
    #[allow(unsafe_code)]
    unsafe {
        mi_option_set(PURGE_DELAY, -1);
    }
}

/// The purge delay mimalloc is using, for the test that checks the option is the one meant.
#[cfg(test)]
fn purge_delay() -> c_long {
    // SAFETY: reading an option has no precondition.
    #[allow(unsafe_code)]
    unsafe {
        mi_option_get(PURGE_DELAY)
    }
}

/// The alignment mimalloc gives a block without being asked, `MI_MAX_ALIGN_SIZE` in its header.
const GIVEN: usize = 16;

/// Whether a plain allocation of this size is already aligned enough for this alignment.
///
/// The two tests mimalloc's own `mi_malloc_is_naturally_aligned` makes before deciding the same
/// thing, in the same order, so that the answer here is the answer it would have reached. The
/// second of them looks redundant and is not. A block narrower than its own alignment is not one
/// mimalloc promises anything about, and while every Rust type has a size that is a multiple of its
/// alignment, a `Layout` is not required to have come from a type.
const fn given(size: usize, align: usize) -> bool {
    align <= GIVEN && align <= size
}

/// mimalloc, with the alignment decided at the call rather than inside the library.
#[derive(Debug)]
pub(crate) struct MiMalloc;

// SAFETY: every method below hands its arguments to mimalloc and returns what mimalloc returns, so
// the blocks are mimalloc's blocks and they are freed by the one call that takes one back. What
// this implementation adds is the choice of entry point, and [`given`] is the whole of it: a size
// and alignment it accepts are ones mimalloc itself would have served from the plain allocator
// after reaching the same conclusion, so the pointer satisfies the layout it was asked for.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for MiMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: mimalloc takes any size, and [`given`] picks the call that answers this
        // alignment.
        unsafe {
            if given(layout.size(), layout.align()) {
                mi_malloc(layout.size()).cast()
            } else {
                mi_malloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as [`GlobalAlloc::alloc`], and the zeroing is mimalloc's own.
        unsafe {
            if given(layout.size(), layout.align()) {
                mi_zalloc(layout.size()).cast()
            } else {
                mi_zalloc_aligned(layout.size(), layout.align()).cast()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: the pointer came from one of the calls above, all four of which are mimalloc's,
        // and `mi_free` is how mimalloc takes a block back whichever of them made it.
        unsafe { mi_free(ptr.cast()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the pointer is mimalloc's and `layout.align()` is the alignment it still has to
        // have. The smaller of the two sizes is the one asked about, because the block has to be
        // aligned enough both as it is and as it will be, and the plain call is only taken when
        // both of those are a size mimalloc aligns anyway.
        unsafe {
            if given(layout.size().min(size), layout.align()) {
                mi_realloc(ptr.cast(), size).cast()
            } else {
                mi_realloc_aligned(ptr.cast(), size, layout.align()).cast()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GIVEN, given, keep_freed_memory, purge_delay};

    /// The option set is the purge delay, which mimalloc 2 starts at ten milliseconds, and not some
    /// other entry of the enum the constant could have drifted to.
    #[test]
    fn keeping_freed_memory_turns_the_purge_delay_off() {
        if std::env::var_os("MIMALLOC_PURGE_DELAY").is_some() {
            return;
        }
        assert_eq!(purge_delay(), 10, "mimalloc's own default");
        keep_freed_memory();
        assert_eq!(purge_delay(), -1);
    }

    /// The alignments the engine actually asks for take the plain call, and the ones that would not
    /// be served by it do not.
    #[test]
    fn an_alignment_mimalloc_gives_anyway_is_not_asked_for() {
        assert!(given(16_384, 8), "a column of two thousand i64");
        assert!(given(32_768, 16), "a column of two thousand i128");
        assert!(given(1, 1), "a byte");
        assert!(given(GIVEN, GIVEN), "a block exactly its own alignment");

        assert!(!given(4_096, 64), "a cache line aligned block still has to ask");
        assert!(!given(8, GIVEN), "a block narrower than its alignment still has to ask");
        assert!(!given(0, 1), "and a block of nothing is nobody's fast path");
    }
}
