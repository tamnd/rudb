//! Freed memory back to the system, for the build on glibc's allocator.
//!
//! glibc keeps what a thread frees in that thread's arena and gives back only what is at the top of
//! it, so a load that frees a stripe's buffers between blocks still in use grows well past what it
//! holds. `malloc_trim` walks every arena and hands back the whole free pages inside them too. The
//! library says when, through `rudb::heap`, and this says how. See `rudb_common::heap` for the
//! measurement.
//!
//! A trim is not free. A page handed back is a page the kernel clears again when it is next taken,
//! and a load takes most of what it frees again within a stripe. So this asks glibc how much it is
//! holding free first and trims only past [`KEEP`], which leaves a load that was never going to
//! grow alone.

use std::ffi::c_int;

/// How much free memory glibc may hold before a release trims it.
const KEEP: usize = 256 << 20;

/// `struct mallinfo2` in glibc's `malloc.h`, since 2.33. Only `fordblks` is read.
#[repr(C)]
struct Mallinfo2 {
    arena: usize,
    ordblks: usize,
    smblks: usize,
    hblks: usize,
    hblkhd: usize,
    usmblks: usize,
    fsmblks: usize,
    uordblks: usize,
    /// The bytes held free in every arena, which is what a trim can give back at most.
    fordblks: usize,
    keepcost: usize,
}

// In glibc's `malloc.h`. Not in the `libc` crate's list for every target, and two declarations are
// cheaper than a dependency.
#[allow(unsafe_code)]
unsafe extern "C" {
    fn mallinfo2() -> Mallinfo2;
    fn malloc_trim(pad: usize) -> c_int;
}

/// Hands back every free page glibc is holding, when it is holding more than [`KEEP`].
fn trim() {
    // SAFETY: both calls take plain values and touch only the allocator's own state, under the
    // allocator's own locks. Either is safe to call from any thread at any time.
    #[allow(unsafe_code)]
    unsafe {
        if mallinfo2().fordblks >= KEEP {
            malloc_trim(0);
        }
    }
}

/// Registers [`trim`] as what the library's release does.
pub(crate) fn install() {
    rudb::heap::on_release(trim);
}
