//! Telling the system allocator to keep the pages a query gave back, rather than returning them.
//!
//! This is here for the same reason [`crate::Pool`] is here. The pool exists because a thread that
//! is made when a query starts and joined when it ends costs about sixteen microseconds each way,
//! which is a large fraction of a query that takes a millisecond, and the fix is to keep the thread
//! rather than to make it faster. Memory has the same shape and a larger number on it, and the fix
//! is the same one.
//!
//! # What the measurement said
//!
//! ClickBench query 16 on the million row file took two hundred thousand minor page faults over
//! twenty runs, which is about ten thousand a run, which is forty megabytes of pages the kernel had
//! to find and zero before the query could write into them. Its system time was 0.24 seconds
//! against 0.58 seconds of user time, so a third of everything that query spent on a CPU was the
//! kernel handing it memory it had just given back. Callgrind is what sent us looking: it said one
//! run of query 25 was a hundred and fifty five million instructions against seven and a half
//! milliseconds of CPU, which would be twenty billion instructions a second on one core, and no
//! core does that. The instructions were not the cost. The pages were.
//!
//! The reason is the default glibc policy, which is a reasonable one for a program that allocates a
//! few large things and keeps them. Anything over a hundred and twenty eight kilobytes is served by
//! `mmap` and given straight back to the kernel on free, and the top of the heap is trimmed back
//! whenever there is more than a hundred and twenty eight kilobytes of it free. A query engine is
//! the other kind of program. It allocates a scatter buffer of twenty four megabytes, a group table
//! of a few more, a chunk of vectors per operator per instance, and it frees all of them at the end
//! of the query and asks for the same sizes again on the next one. Under the default policy every
//! one of those is a fresh mapping of zero pages, faulted in one page at a time on first touch.
//!
//! # What this does about it
//!
//! Three numbers, set once per process. A block of a few megabytes or more comes from the heap
//! rather than from its own mapping, so freeing one leaves the pages mapped and the next query
//! writes into pages that are already there. The top of the heap is held rather than given back
//! until there is a lot of it free. And when the heap does have to grow it grows by more than was
//! asked for, so a query that climbs to its peak in many steps pays for one growth rather than many.
//!
//! All three or none. Setting one of them tells glibc to stop adjusting the others on its own, and
//! its own adjustment is not a bad one, so one alone is worse than nothing at all. Query 16 over
//! twenty runs takes 191,013 minor faults on the default, 224,195 with the trim threshold set on its
//! own and 275,658 with the mapping threshold set on its own, against 38,865 with all three.
//!
//! # Where the three numbers came from
//!
//! A sweep, because guessing was wrong twice. The first version of this asked for a trim threshold
//! of 256 MB, a top pad of 64 MB and a mapping threshold of 32 MB, which is glibc's ceiling for that
//! one, and that is 2 percent slower over the suite than what is here while holding 40 MB more. The
//! second guess went the other way, down to a 2 MB mapping threshold and a 16 MB trim, and that is 3
//! percent slower again. Holding blocks far larger than any a query asks for buys nothing and
//! fragments the arena each worker thread allocates out of. Holding blocks smaller than the ones a
//! query does ask for hands those back to the kernel every time.
//!
//! So the mapping threshold sits just above the largest block a radix partition asks for and the
//! top pad just above what one query adds to the heap, and the trim threshold is the one that wants
//! to be large, because it is what decides whether the heap a query grew is still there for the next
//! one.
//!
//! # What it costs
//!
//! Resident memory, and it is real rather than free. A process that has run one large query holds
//! the pages that query used instead of returning them, so over twenty runs query 16 goes from
//! 99 MB to 170 MB, query 18 from 105 MB to 160 MB, query 32 from 154 MB to 156 MB and query 28
//! from 251 MB to 488 MB. DuckDB on the same machine is 506 MB on query 16 and 481 MB on query 28,
//! so on every query but that last one this spends part of a lead rather than giving one up, and on
//! query 28 it gives the lead up. That query is the one to come back to, because what it holds is a
//! `String` per group for ninety five thousand groups that a `HAVING` then throws away, and a
//! smaller peak there is worth more than a smaller threshold here.
//!
//! For a database, holding pages is the right way round, and it is what every serious engine does,
//! usually by linking a different allocator entirely. The dependency rule here means asking the one
//! we have.
//!
//! # Where this works and where it does nothing
//!
//! `mallopt` is a glibc call. On musl, on macOS and on Windows this compiles to nothing at all and
//! the allocator does whatever it was going to do, which on macOS is already closer to what we want.
//! Nothing here is required for correctness, so a target that does not have it loses some speed and
//! no answers.

/// Ask the system allocator to hold on to large blocks between queries.
///
/// Call it before the first query and not once per query. It is idempotent and cheap, so calling it
/// twice is not a bug, but it changes a process wide setting, which is a thing a library should do
/// deliberately and in one place rather than incidentally.
///
/// Reports whether anything was changed, which is false on every target that is not glibc and on a
/// glibc that refused one of the three. Nothing in the engine reads the answer. It is returned so
/// that a test can say what happened and a caller that cares can log it.
pub fn keep_pages() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        glibc::keep_pages()
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
// The crate denies unsafe and this is the second place that asks for it. The first is the parked
// worker in `pool.rs`, which has an invariant to uphold. This one has a signature to get right.
#[allow(unsafe_code, reason = "the mallopt declaration, which is the only way to ask glibc this")]
mod glibc {
    //! The three `mallopt` parameters and the one call that sets them.

    /// Do not return the top of the heap to the system until this much of it is free.
    ///
    /// Negative because glibc numbers its `mallopt` parameters from minus one downwards, which is
    /// how it keeps them out of the way of the ones other systems define.
    const M_TRIM_THRESHOLD: i32 = -1;

    /// Grow the heap by this much more than was asked for whenever it has to grow.
    const M_TOP_PAD: i32 = -2;

    /// Serve an allocation this size or larger with its own mapping rather than from the heap.
    const M_MMAP_THRESHOLD: i32 = -3;

    /// How much free space the top of the heap is allowed to hold before it is given back.
    ///
    /// Two hundred and fifty six megabytes, which is more than the peak of any query in ClickBench
    /// on the million row file, so for that suite the answer is never. This is the one of the three
    /// that wants to be large, and it is the one the resident size is paid to: a smaller trim
    /// threshold gives the same faults back one query later.
    const TRIM_THRESHOLD: i32 = 256 * 1024 * 1024;

    /// How much more than it was asked for the heap grows by.
    ///
    /// Eight megabytes. This is address space rather than pages: the kernel does not back any of it
    /// until it is written to, so the cost of asking for a little too much is nothing and the cost
    /// of asking for too little is another system call per growth. Sixty four measured no faster
    /// and held 50 MB more on query 28, so eight.
    const TOP_PAD: i32 = 8 * 1024 * 1024;

    /// The size at which an allocation gets its own mapping.
    ///
    /// Four megabytes, which is above every allocation a radix partition makes and below the handful
    /// a query makes once. glibc takes anything up to thirty two megabytes here and refuses rather
    /// than clamps above that, but thirty two measured slower and held more: see the module
    /// documentation.
    const MMAP_THRESHOLD: i32 = 4 * 1024 * 1024;

    unsafe extern "C" {
        /// Sets one allocator parameter, and returns non zero when it took.
        fn mallopt(parameter: i32, value: i32) -> i32;
    }

    /// Sets all three, and reports whether every one of them took.
    pub(super) fn keep_pages() -> bool {
        let settings = [
            (M_TRIM_THRESHOLD, TRIM_THRESHOLD),
            (M_TOP_PAD, TOP_PAD),
            (M_MMAP_THRESHOLD, MMAP_THRESHOLD),
        ];
        let mut all = true;
        for (parameter, value) in settings {
            // SAFETY: `mallopt` takes two integers by value, returns one, and touches nothing the
            // caller owns. The parameter numbers and the signature are glibc's, which the
            // conditions on this module make the allocator in use, and both values are inside the
            // range glibc documents for their parameter.
            all &= unsafe { mallopt(parameter, value) } != 0;
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::keep_pages;

    #[test]
    fn keeping_pages_works_on_glibc_and_is_harmless_everywhere_else() {
        let changed = keep_pages();
        // Calling it twice is what a second database in the same process does, and it has to be
        // safe and has to give the same answer.
        assert_eq!(changed, keep_pages());
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert!(changed, "glibc refused one of the three settings");
        } else {
            assert!(!changed, "something was changed on a target that has no mallopt");
        }
    }
}
