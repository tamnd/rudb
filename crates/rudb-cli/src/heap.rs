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
//!
//! A block of 256 KiB or more does not go to mimalloc at all on Linux. It is mapped from the system
//! on its own, grown with `mremap`, and unmapped when it is freed, or held for the next block of
//! its length; [`MAPPED_FROM`] and [`HELD_UPTO`] say why.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::{c_int, c_long};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use libmimalloc_sys::{
    mi_free, mi_malloc, mi_malloc_aligned, mi_realloc, mi_realloc_aligned, mi_zalloc,
    mi_zalloc_aligned,
};

/// `mi_option_purge_delay` in mimalloc 2's `mimalloc.h`, the sixteenth entry of `mi_option_e`.
const PURGE_DELAY: c_int = 15;

/// How long freed memory is held before it goes back to the system, see [`keep_freed_memory`].
const HELD_FOR_MS: c_long = 100;

// The two calls of mimalloc's option interface this needs. `libmimalloc-sys` declares them only
// behind a feature that also pulls in a dependency, and they are in the library it links either
// way, so they are declared here as they are in the header.
#[allow(unsafe_code)]
unsafe extern "C" {
    fn mi_option_set(option: c_int, value: c_long);
    #[cfg(test)]
    fn mi_option_get(option: c_int) -> c_long;
}

/// Tells mimalloc to hold freed memory for a tenth of a second before it gives it back.
///
/// By default mimalloc returns freed memory ten milliseconds after it is freed. A load frees and
/// takes gigabytes over and over inside one statement: the sorted SF1 `lineitem` CTAS peaks at
/// 2.3GB and mimalloc's own statistics show it purging 4.7GiB along the way, every byte of which
/// is faulted back in when it is taken again. So #1429 turned the purge off, which over 21
/// interleaved runs took about 7 percent off that statement's median.
///
/// Off was too far for a bigger load. One that builds and drops a stripe's worth of buffers on
/// thirty two threads at once keeps most of what it dropped, and the ClickBench 100m load on the 32
/// core gamingpc peaked at 20 to 21 GB on a 31 GB machine with the purge off. At 100 milliseconds
/// it peaked at 14.4 to 15.1 GB, and the 10m load at 4.7 to 5.5 GB against 7.5, in the same wall
/// time, 5.81 to 5.88 seconds against 5.83 to 5.92, and about 8 seconds of system time against 5 to
/// 8. The 43 ClickBench queries on the 10m file summed to 0.294 to 0.299 seconds either way, and
/// the `lineitem` CTAS had the same median, 2.66 seconds into a file and 0.53 in memory. The
/// default of ten milliseconds held less again, 4.1 GB on the 10m load, but took nearly twice the
/// system time on gamingpc and three times on the eight core server3, where 100 milliseconds took
/// twice. `MIMALLOC_PURGE_DELAY` in the environment still decides, for a user who wants a different
/// trade.
pub(crate) fn keep_freed_memory() {
    if std::env::var_os("MIMALLOC_PURGE_DELAY").is_some() {
        return;
    }
    // SAFETY: an option is an integer mimalloc reads when it next decides whether to purge, and
    // setting one is allowed at any time, including after the first allocation.
    #[allow(unsafe_code)]
    unsafe {
        mi_option_set(PURGE_DELAY, HELD_FOR_MS);
    }
}

/// Tells mimalloc not to give anything back from here on, which is for the moment before the
/// process exits.
///
/// mimalloc's exit handler collects every heap with the purge forced, and a forced purge hands
/// back every page still held with `madvise`, one page table entry at a time, moments before the
/// kernel takes the whole address space back anyway. On ClickBench q41 over the native 10m table
/// that was a quarter of the samples in the process, 265 MB decommitted by the main thread after
/// the answer was already written. A purge delay below zero is mimalloc's way of saying never
/// purge, and the collect at exit reads it.
pub(crate) fn keep_everything_at_exit() {
    // SAFETY: as in [`keep_freed_memory`].
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

/// The smallest block that is rounded, one past a kilobyte.
///
/// A bound of 512 bytes, tried with the upper bound at 2 MiB, held no less on ClickBench.
const ROUNDED_FROM: usize = 1024 + 1;

/// The largest block mimalloc 2 serves from a medium page, `MI_MEDIUM_OBJ_SIZE_MAX`, and the
/// largest that is rounded. A larger block has a span of its own rather than a slot in a page.
const ROUNDED_UPTO: usize = 128 * 1024;

/// The size to ask mimalloc for when the caller wants `size`, which is `size` rounded up to a power
/// of two from a kilobyte to 128 KiB and `size` otherwise.
///
/// mimalloc keeps a page per size class per thread, and there are four classes to every doubling.
/// A medium page, for blocks from 16 KiB to 128 KiB, is 512 KiB, so the twelve classes there can
/// hold six megabytes a thread that is committed and mostly empty. A vector is 8192 values, which
/// puts most of what a scan takes and frees in exactly that range, at widths of one, two, four,
/// eight and sixteen bytes and at every string length in between. On ClickBench q10 at eight
/// threads the live heap peaked at 39 MB and the process at 71. A small page is 64 KiB and the
/// sixteen classes from a kilobyte to 16 KiB cost less each, but q10 takes and frees nearly
/// thirty thousand blocks there, and rounding them as well took another 33 MB off the 43 ClickBench
/// queries over the native file and 55 MB over Parquet. Rounding leaves seven classes in the whole
/// range, and a block that grows into the slack is grown where it is.
const fn binned(size: usize) -> usize {
    if size >= ROUNDED_FROM && size <= ROUNDED_UPTO { size.next_power_of_two() } else { size }
}

/// The smallest block mapped from the system on its own rather than taken from mimalloc, on Linux.
///
/// The big blocks of a query are vectors that grow by doubling, an aggregate's accumulators or a
/// partition's group table, and the copies they leave behind. mimalloc grows a block that size by
/// taking a new one, copying and freeing the old one, and holds what was freed for its purge delay,
/// a tenth of a second, which is as long as most of a query runs. On ClickBench q29 at eight
/// threads the live heap peaked at 177 MB and the process at 348, against 193 and 230 on one
/// thread, and with the purge delay at zero it was 210. A mapping grows with `mremap`, which moves
/// the pages rather than copying them, the slack past what was written is never touched, and an
/// unmapped block is gone at once without the purge of every small page that a zero delay costs.
///
/// Over the 43 ClickBench queries, with the same binary and only this bound changed, 256 KiB took
/// 81 MB off the summed peak over the native file and 51 MB over Parquet, 512 KiB 62 and 44, and
/// 1 MiB 37 and 21. Below 256 KiB a block is one of mimalloc's medium or small ones, which the
/// rounding above already keeps in few classes.
const MAPPED_FROM: usize = 256 * 1024;

/// Whether blocks are mapped on their own on this target. The constants in [`system`] are the ones
/// of the Linux targets named here.
const MAPS: bool =
    cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")));

/// The smallest page size Linux has, and so an alignment every mapping has.
const PAGE: usize = 4096;

/// The length of the mapping a block of `size` bytes is given: `size` rounded up to a quarter of
/// the power of two at or above it, and so to a page from [`MAPPED_FROM`] up.
///
/// A pure function of the size, like [`mapped`], so that the call that frees or grows a block
/// unmaps the length the call that made it mapped. Four lengths to a doubling is what lets a freed
/// block be taken again by the next one near its size, and what it costs is address space: the
/// slack past `size` is never touched unless the block grows into it, which it then does in place.
const fn span(size: usize) -> usize {
    let step = size.next_power_of_two() / 4;
    let step = if step < PAGE { PAGE } else { step };
    size.div_ceil(step) * step
}

/// The most freed mapped memory held for the next block of the same length.
///
/// A query takes and frees its big blocks over and over: the bitmaps a consistent node grows to its
/// largest key, the adjacency a gather builds, the text a `LIKE` copies out to search. Each one was
/// a fresh mapping, so every page of it was a fault the kernel answered with a cleared page, and
/// every unmap stopped the other threads to drop the page from their TLBs. Over the 113 JOB queries
/// on server2 a third of rudb's cycles were the kernel's, against a twelfth of DuckDB's, and most
/// of them were those faults and unmaps. A block held here is taken again with its pages still in.
const HELD_UPTO: usize = 128 << 20;

/// How many freed mappings are held at once.
const HELD_SLOTS: usize = 64;

/// The freed mappings held for reuse, under a lock that is only ever held to look through them.
///
/// Atomics under a spin lock rather than a `Mutex`, because this is the allocator and a lock that
/// can allocate or park cannot be taken here. A block this big is a few a query, so the lock is
/// never contended long.
mod held {
    use super::{AtomicBool, AtomicPtr, AtomicUsize, HELD_SLOTS, HELD_UPTO, Ordering};

    static LOCK: AtomicBool = AtomicBool::new(false);
    static BLOCKS: [AtomicPtr<u8>; HELD_SLOTS] =
        [const { AtomicPtr::new(std::ptr::null_mut()) }; HELD_SLOTS];
    static LENGTHS: [AtomicUsize; HELD_SLOTS] = [const { AtomicUsize::new(0) }; HELD_SLOTS];
    static HELD: AtomicUsize = AtomicUsize::new(0);

    fn locked<T>(work: impl FnOnce() -> T) -> T {
        while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
        {
            std::hint::spin_loop();
        }
        let done = work();
        LOCK.store(false, Ordering::Release);
        done
    }

    /// A held mapping of exactly `length` bytes, or null.
    pub(super) fn take(length: usize) -> *mut u8 {
        if HELD.load(Ordering::Relaxed) < length {
            return std::ptr::null_mut();
        }
        locked(|| {
            for (block, held) in BLOCKS.iter().zip(&LENGTHS) {
                if held.load(Ordering::Relaxed) == length {
                    held.store(0, Ordering::Relaxed);
                    HELD.fetch_sub(length, Ordering::Relaxed);
                    return block.swap(std::ptr::null_mut(), Ordering::Relaxed);
                }
            }
            std::ptr::null_mut()
        })
    }

    /// Holds a mapping of `length` bytes, or says it was not held because that would hold more
    /// than [`HELD_UPTO`] or every slot is taken.
    pub(super) fn keep(block: *mut u8, length: usize) -> bool {
        if HELD.load(Ordering::Relaxed) + length > HELD_UPTO {
            return false;
        }
        locked(|| {
            if HELD.load(Ordering::Relaxed) + length > HELD_UPTO {
                return false;
            }
            for (slot, held) in BLOCKS.iter().zip(&LENGTHS) {
                if held.load(Ordering::Relaxed) == 0 {
                    slot.store(block, Ordering::Relaxed);
                    held.store(length, Ordering::Relaxed);
                    HELD.fetch_add(length, Ordering::Relaxed);
                    return true;
                }
            }
            false
        })
    }

    /// Every held mapping, let go of, for the caller to unmap.
    pub(super) fn drain(mut each: impl FnMut(*mut u8, usize)) {
        let mut taken = [(std::ptr::null_mut(), 0); HELD_SLOTS];
        locked(|| {
            for ((slot, held), taken) in BLOCKS.iter().zip(&LENGTHS).zip(&mut taken) {
                *taken = (
                    slot.swap(std::ptr::null_mut(), Ordering::Relaxed),
                    held.swap(0, Ordering::Relaxed),
                );
            }
            HELD.store(0, Ordering::Relaxed);
        });
        for (block, length) in taken {
            if length != 0 {
                each(block, length);
            }
        }
    }
}

/// A mapping of `size` bytes, a held one if there is one of its length.
fn map(size: usize) -> *mut u8 {
    let length = span(size);
    let block = held::take(length);
    if block.is_null() { system::map(length) } else { block }
}

/// Lets go of a mapping of `size` bytes, held for the next block if there is room.
///
/// # Safety
///
/// `block` is a mapping [`map`] made for `size` bytes, or one grown to it, and nothing reads it
/// after.
#[allow(unsafe_code)]
unsafe fn unmap(block: *mut u8, size: usize) {
    let length = span(size);
    if !held::keep(block, length) {
        // SAFETY: the caller's.
        unsafe { system::unmap(block, length) };
    }
}

/// Unmaps every held mapping, which is what a release asks for: a load lets go of a stripe's
/// buffers at once and is the moment holding them costs the most.
pub(crate) fn release() {
    // SAFETY: a held mapping is one nothing holds any more, and draining takes it out of the slots
    // before it is unmapped.
    #[allow(unsafe_code)]
    held::drain(|block, length| unsafe { system::unmap(block, length) });
}

/// Whether a block of this size and alignment is a mapping of its own.
///
/// A pure function of the layout, so that the call that frees or grows a block, which is given the
/// layout it was made with, reaches the same answer the call that made it did.
const fn mapped(size: usize, align: usize) -> bool {
    MAPS && size >= MAPPED_FROM && align <= PAGE
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod system {
    use std::ffi::{c_int, c_void};

    const PROT_READ_WRITE: c_int = 0x1 | 0x2;
    const MAP_PRIVATE_ANONYMOUS: c_int = 0x02 | 0x20;
    const MREMAP_MAYMOVE: c_int = 0x1;
    const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

    // Declared as in the C library's headers, for the reason the two options above are.
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn mmap(
            addr: *mut c_void,
            length: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, length: usize) -> c_int;
        fn mremap(
            old: *mut c_void,
            old_length: usize,
            new_length: usize,
            flags: c_int,
            ...
        ) -> *mut c_void;
    }

    /// A fresh zeroed mapping of at least `size` bytes, or null.
    pub(super) fn map(size: usize) -> *mut u8 {
        // SAFETY: an anonymous private mapping at an address of the kernel's choosing touches no
        // memory the program holds.
        #[allow(unsafe_code)]
        let block = unsafe {
            mmap(std::ptr::null_mut(), size, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS, -1, 0)
        };
        if block == MAP_FAILED { std::ptr::null_mut() } else { block.cast() }
    }

    /// Gives back a mapping [`map`] or [`remap`] made of `size` bytes.
    ///
    /// # Safety
    ///
    /// `block` is such a mapping and nothing reads it after.
    #[allow(unsafe_code)]
    pub(super) unsafe fn unmap(block: *mut u8, size: usize) {
        // SAFETY: the caller's. The kernel rounds the length up to the page as it did at the map.
        unsafe {
            munmap(block.cast(), size);
        }
    }

    /// The mapping of `old` bytes at `block` resized to `new`, moved if it has to be, or null with
    /// the old one left as it was.
    ///
    /// # Safety
    ///
    /// `block` is a mapping [`map`] or [`remap`] made of `old` bytes.
    #[allow(unsafe_code)]
    pub(super) unsafe fn remap(block: *mut u8, old: usize, new: usize) -> *mut u8 {
        // SAFETY: the caller's, and a failed resize leaves the mapping where it was.
        let moved = unsafe { mremap(block.cast(), old, new, MREMAP_MAYMOVE) };
        if moved == MAP_FAILED { std::ptr::null_mut() } else { moved.cast() }
    }
}

/// The stand in where nothing is mapped, which [`mapped`] never lets anything reach.
#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod system {
    pub(super) fn map(_size: usize) -> *mut u8 {
        std::ptr::null_mut()
    }

    #[allow(unsafe_code)]
    pub(super) unsafe fn unmap(_block: *mut u8, _size: usize) {}

    #[allow(unsafe_code)]
    pub(super) unsafe fn remap(_block: *mut u8, _old: usize, _new: usize) -> *mut u8 {
        std::ptr::null_mut()
    }
}

/// mimalloc, with the alignment decided at the call rather than inside the library.
#[derive(Debug)]
pub(crate) struct MiMalloc;

// SAFETY: every method below hands its arguments to mimalloc and returns what mimalloc returns, so
// the blocks are mimalloc's blocks and they are freed by the one call that takes one back. What
// this implementation adds is the choice of entry point, and [`given`] is the whole of it: a size
// and alignment it accepts are ones mimalloc itself would have served from the plain allocator
// after reaching the same conclusion, so the pointer satisfies the layout it was asked for. The
// exception is a block [`mapped`] accepts, which is a mapping of its own: page aligned, so aligned
// for any alignment up to a page, which is all `mapped` accepts, and zeroed by the kernel. Whether
// a block is one is decided from its layout alone, so it is freed and grown by the calls that
// match the one that made it.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for MiMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if mapped(layout.size(), layout.align()) {
            return map(layout.size());
        }
        // SAFETY: mimalloc takes any size, and [`given`] picks the call that answers this
        // alignment.
        unsafe {
            if given(layout.size(), layout.align()) {
                mi_malloc(binned(layout.size())).cast()
            } else {
                mi_malloc_aligned(binned(layout.size()), layout.align()).cast()
            }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if mapped(layout.size(), layout.align()) {
            let length = span(layout.size());
            let block = held::take(length);
            if block.is_null() {
                // A fresh mapping is zeroed by the kernel.
                return system::map(length);
            }
            // SAFETY: a held block is a mapping of `length` bytes, at least the size asked for.
            unsafe { block.write_bytes(0, layout.size()) };
            return block;
        }
        // SAFETY: as [`GlobalAlloc::alloc`], and the zeroing is mimalloc's own.
        unsafe {
            if given(layout.size(), layout.align()) {
                mi_zalloc(binned(layout.size())).cast()
            } else {
                mi_zalloc_aligned(binned(layout.size()), layout.align()).cast()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer came from one of the calls above with this layout. A mapped one is
        // unmapped, and the rest are mimalloc's, which `mi_free` takes back whichever call made
        // them.
        unsafe {
            if mapped(layout.size(), layout.align()) {
                unmap(ptr, layout.size());
            } else {
                mi_free(ptr.cast());
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the pointer is mimalloc's and `layout.align()` is the alignment it still has to
        // have. The smaller of the two sizes is the one asked about, because the block has to be
        // aligned enough both as it is and as it will be, and the plain call is only taken when
        // both of those are a size mimalloc aligns anyway.
        unsafe {
            let was = mapped(layout.size(), layout.align());
            let will = mapped(size, layout.align());
            if was && will {
                let (old, new) = (span(layout.size()), span(size));
                return if old == new { ptr } else { system::remap(ptr, old, new) };
            }
            if was || will {
                // Across the bound the block changes hands, so it is copied into one the other
                // side made, and the old one is kept if that fails, as `realloc` promises.
                let grown = Layout::from_size_align_unchecked(size, layout.align());
                let moved = self.alloc(grown);
                if !moved.is_null() {
                    std::ptr::copy_nonoverlapping(ptr, moved, layout.size().min(size));
                    self.dealloc(ptr, layout);
                }
                return moved;
            }
            if given(layout.size().min(size), layout.align()) {
                mi_realloc(ptr.cast(), binned(size)).cast()
            } else {
                mi_realloc_aligned(ptr.cast(), binned(size), layout.align()).cast()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout};

    use super::{
        GIVEN, MAPPED_FROM, MAPS, MiMalloc, PAGE, ROUNDED_FROM, ROUNDED_UPTO, binned, given,
        keep_everything_at_exit, keep_freed_memory, mapped, purge_delay, release, span,
    };

    /// The option set is the purge delay, which mimalloc 2 starts at ten milliseconds, and not some
    /// other entry of the enum the constant could have drifted to, and the one set at exit turns
    /// purging off.
    #[test]
    fn keeping_freed_memory_sets_the_purge_delay() {
        if std::env::var_os("MIMALLOC_PURGE_DELAY").is_some() {
            return;
        }
        assert_eq!(purge_delay(), 10, "mimalloc's own default");
        keep_freed_memory();
        assert_eq!(purge_delay(), 100);
        // In the same test rather than one of its own, because the option is process wide and the
        // tests run at once.
        keep_everything_at_exit();
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

    /// A block from a kilobyte to 128 KiB is rounded up to a power of two and nothing else is
    /// touched.
    #[test]
    fn only_a_block_from_a_kilobyte_to_a_medium_one_is_rounded() {
        assert_eq!(binned(ROUNDED_FROM - 1), ROUNDED_FROM - 1, "a kilobyte is already a class");
        assert_eq!(binned(ROUNDED_FROM), 2 * 1024);
        assert_eq!(binned(5 * 1024), 8 * 1024, "a selection of 1280 rows");
        assert_eq!(binned(40 * 1024), 64 * 1024, "a column of five thousand i64");
        assert_eq!(binned(64 * 1024), 64 * 1024, "a column of 8192 i64 is already a class");
        assert_eq!(binned(ROUNDED_UPTO), ROUNDED_UPTO);
        assert_eq!(
            binned(ROUNDED_UPTO + 1),
            ROUNDED_UPTO + 1,
            "a large block has a span of its own"
        );
        assert_eq!(binned(0), 0);
    }

    /// Only a block from the bound up, at no more than a page of alignment, is mapped, and only
    /// where mapping is on.
    #[test]
    fn only_a_big_block_is_mapped() {
        assert_eq!(mapped(MAPPED_FROM, 8), MAPS);
        assert_eq!(mapped(64 << 20, PAGE), MAPS, "a page of alignment is a mapping's own");
        assert!(!mapped(MAPPED_FROM - 1, 8), "a medium block stays with mimalloc");
        assert!(!mapped(MAPPED_FROM, 2 * PAGE), "and so does one that wants more than a page");
        assert!(!mapped(0, 1));
    }

    /// A block keeps what was written to it as it grows within the mappings, shrinks back across
    /// the bound to mimalloc and grows across it again, and a zeroed one reads as zeros.
    #[test]
    #[allow(unsafe_code)]
    fn a_block_keeps_its_bytes_across_the_bound() {
        let heap = MiMalloc;
        let fill = |block: *mut u8, size: usize| {
            for at in (0..size).step_by(4093) {
                // SAFETY: `at` is inside the block of `size` bytes.
                unsafe { block.add(at).write((at % 251) as u8) };
            }
        };
        let check = |block: *const u8, size: usize| {
            for at in (0..size).step_by(4093) {
                // SAFETY: as in `fill`.
                assert_eq!(unsafe { block.add(at).read() }, (at % 251) as u8, "byte {at}");
            }
        };
        // SAFETY: each call is given the layout the block has at that point.
        unsafe {
            let small = 100 * 1024;
            let layout = Layout::from_size_align(MAPPED_FROM + 3, 8).expect("a layout");
            let block = heap.alloc_zeroed(layout);
            assert!(!block.is_null());
            assert!((0..layout.size()).step_by(997).all(|at| block.add(at).read() == 0));
            fill(block, layout.size());
            let big = 8 << 20;
            let block = heap.realloc(block, layout, big);
            assert!(!block.is_null());
            check(block, layout.size());
            fill(block, big);
            let layout = Layout::from_size_align(big, 8).expect("a layout");
            let block = heap.realloc(block, layout, small);
            assert!(!block.is_null());
            check(block, small);
            let layout = Layout::from_size_align(small, 8).expect("a layout");
            let block = heap.realloc(block, layout, MAPPED_FROM * 2);
            assert!(!block.is_null());
            check(block, small);
            heap.dealloc(block, Layout::from_size_align(MAPPED_FROM * 2, 8).expect("a layout"));
        }
    }

    /// A mapping is four lengths to a doubling, and a freed one is taken again, cleared when it is
    /// asked for zeroed, by the next block of its length.
    #[test]
    #[allow(unsafe_code)]
    fn a_freed_mapping_is_taken_again() {
        assert_eq!(span(MAPPED_FROM), MAPPED_FROM);
        assert_eq!(span(MAPPED_FROM + 1), MAPPED_FROM / 2 * 3);
        assert_eq!(span(300 * 1024), 384 * 1024);
        assert_eq!(span(1 << 20), 1 << 20);
        assert_eq!(span((1 << 20) + 1), 3 << 19);
        assert!((MAPPED_FROM..64 << 20).step_by(4093).all(|size| span(size) % PAGE == 0));
        if !MAPS {
            return;
        }
        let heap = MiMalloc;
        // A length no other test here asks for, since the held blocks are the process's.
        let layout = Layout::from_size_align((3 << 20) + 5, 8).expect("a layout");
        // SAFETY: each block is freed with the layout it was made with.
        unsafe {
            let block = heap.alloc(layout);
            assert!(!block.is_null());
            block.write_bytes(7, layout.size());
            heap.dealloc(block, layout);
            let again = heap.alloc_zeroed(layout);
            assert_eq!(again, block, "the freed mapping is the one taken");
            assert!((0..layout.size()).step_by(997).all(|at| again.add(at).read() == 0));
            heap.dealloc(again, layout);
        }
        release();
    }
}
