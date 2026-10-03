//! The `rudb` shell.
//!
//! # The allocator
//!
//! A bulk load is an allocator benchmark. `CREATE TABLE fact AS SELECT ... FROM range(20000000)`
//! over two columns produces 19,532 chunks on the worker threads, hands them to the one thread
//! draining the query, and frees them there, which is about 39,000 buffers taken on one thread and
//! given back on another. glibc gives each thread its own arena and returns a block to the arena it
//! came from, so almost none of that memory is reachable by the thread doing the taking, and the
//! load spends its time in `malloc_consolidate` and `unlink_chunk` rather than in the engine.
//!
//! Measured on server2, six EPYC cores, 20 million rows over two columns, best of five. The
//! mimalloc column is this build. The jemalloc column is the same binary with no allocator linked
//! in and `LD_PRELOAD` pointing at the system library, which is how the comparison was made.
//!
//! | | glibc | mimalloc | jemalloc |
//! | --- | --- | --- | --- |
//! | load, wall | 0.95 s | 0.44 s | 0.93 s |
//! | load, peak RSS | 336 MB | 336 MB | 344 MB |
//! | `count(*) WHERE k = 7` | 0.0197 s | 0.0179 s | 0.0165 s |
//! | `sum(k + v)` | 0.0860 s | 0.0823 s | 0.0782 s |
//!
//! So the load is a little over twice as fast and the queries are a wash, which is the shape to
//! expect. A scan of a table already in memory takes a chunk's worth and gives it back on the same
//! thread, and every allocator is good at that. What is worth half a second is the case where the
//! producer and the consumer are different threads.
//!
//! jemalloc is in the table because it is what DuckDB ships on Linux and it was the obvious other
//! answer. It recovers two percent of the load, it is the quickest of the three on the queries by a
//! margin too small to choose on, and it holds the most memory, so it is not the one.
//!
//! # Why it was not the default for a while, and is again
//!
//! The table above was taken when a load freed tens of thousands of chunks on a thread other than
//! the one that made them. Measured again later on server3, the system allocator came out ahead.
//! Over the 43 ClickBench queries on the native 10m table, one process per query, glibc took 23.56
//! seconds of CPU against mimalloc's 26.66 and summed to 2448 MB of peak resident memory against
//! 3082, and nearly all of the difference was system time spent on page faults. So for a while the
//! default build installed nothing.
//!
//! The engine has changed again since, and the two suites now agree the other way. On the 42
//! ClickBench queries past the first over the 10m Parquet file on server3, one process per query,
//! glibc took 20.2 seconds of CPU against mimalloc's 19.6, and 37.4 against 34.7 with each query
//! run three times in its process. On JOB, where a session runs many statements, glibc spent about
//! 15 percent of the warm user cycles inside `malloc` and `free` alone, and the 113 queries took
//! 32.20 billion warm user cycles against mimalloc's 30.16. The first run of each JOB query, which
//! is mostly faulting in pages, took 107 seconds of task clock on glibc against 93.5. What
//! mimalloc still costs is memory: the ClickBench peaks summed to 2128 MB against 1764. That is
//! the trade taken, so the `mimalloc` feature is on by default and a build that wants glibc turns
//! default features off.
//!
//! # The two features in the manifest
//!
//! Neither is decoration and both were picked by measuring, because the wrong setting of either
//! costs more than the allocator wins.
//!
//! `v2` pins mimalloc to the 2 series. Without it the crate builds mimalloc 3, which loaded in 0.87
//! seconds against 2's 0.44 and held 416 MB against 336 MB. The 3 series is a rewrite and this is
//! not the workload it came out ahead on.
//!
//! `no_thp` stops mimalloc asking the kernel for transparent huge pages on the memory it reserves.
//! Left on, the load took 0.85 seconds instead of 0.44, and the whole of the difference was system
//! time: 1.80 seconds against 0.45, for the same 73,000 to 82,000 minor faults. A fault that has to
//! be served by a huge page on a machine that has been up long enough to fragment its free memory
//! is a fault that waits for the kernel to compact. The queries are unaffected either way, so there
//! is nothing on the other side of the trade.
//!
//! # Why it is here and not in the library
//!
//! A global allocator is a property of a program. A library that sets one takes the choice away
//! from every program that embeds it, which for `rudb` means the Rust API, the C API and anything
//! linking either, and none of those asked. So the attribute goes on the one binary in the
//! workspace that ships, and only while its `mimalloc` feature is on, which it is by default.

// `deny` rather than `forbid`, and the difference buys exactly one thing. [`heap`] implements a
// trait that cannot be implemented safely, because it is the thing the safe world is built on, and
// it is four calls into a C library. Nothing else in this binary, this crate or the workspace is
// allowed any, and the lint still says so everywhere else.
#![deny(unsafe_code)]

use std::process::ExitCode;

#[cfg(feature = "mimalloc")]
mod heap;
#[cfg(all(not(feature = "mimalloc"), target_os = "linux", target_env = "gnu"))]
mod trim;

/// The allocator every allocation in the shell goes through.
///
/// A unit struct with no state: the attribute is the whole of the wiring, and mimalloc's own
/// initialisation happens on the first allocation rather than here.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: heap::MiMalloc = heap::MiMalloc;

fn main() -> ExitCode {
    #[cfg(feature = "mimalloc")]
    {
        heap::keep_freed_memory();
        rudb::heap::on_release(heap::release);
    }
    #[cfg(all(not(feature = "mimalloc"), target_os = "linux", target_env = "gnu"))]
    trim::install();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let code = rudb_cli::run(&arguments, Box::new(std::io::stdout()), Box::new(std::io::stderr()));
    #[cfg(feature = "mimalloc")]
    heap::keep_everything_at_exit();
    code
}
