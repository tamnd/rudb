//! The two clocks a measured call is timed against.
//!
//! Wall time is [`std::time::Instant`], which is monotonic and needs nothing said about it. CPU
//! time is the one that matters and the one that is awkward: it has to be per thread, because the
//! whole claim this project makes is about CPU seconds rather than wall seconds, and a wall clock
//! ratio can be bought with threads while a CPU one cannot.
//!
//! There is no portable standard library call for per thread CPU time, and the dependency rule
//! means there is no `libc` to ask either, so the one system call is declared here. It is
//! `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, which is the same call the design note names, and it
//! is behind a narrow enough set of conditions that the declaration cannot be wrong: sixty four bit
//! Linux and macOS, where both fields of `timespec` are sixty four bit signed integers. Everywhere
//! else this reports nothing rather than guessing, and nothing is a number a reader can see through
//! while a guess is not.

use std::time::Instant;

/// The clock id for the calling thread's CPU time.
///
/// Different on the two systems this is compiled for, which is the reason it is a constant here
/// rather than a literal at the call.
#[cfg(all(target_pointer_width = "64", any(target_os = "linux", target_os = "android")))]
const THREAD_CPUTIME: i32 = 3;
#[cfg(all(target_pointer_width = "64", any(target_os = "macos", target_os = "ios")))]
const THREAD_CPUTIME: i32 = 16;

#[cfg(all(
    target_pointer_width = "64",
    any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")
))]
mod system {
    /// The two fields `clock_gettime` fills in, both of them sixty four bit signed on every target
    /// this module is compiled for.
    #[repr(C)]
    pub(super) struct Timespec {
        pub(super) seconds: i64,
        pub(super) nanoseconds: i64,
    }

    unsafe extern "C" {
        /// Reads the clock named by `id` into `into`, and returns zero when it worked.
        pub(super) fn clock_gettime(id: i32, into: *mut Timespec) -> i32;
    }
}

/// CPU time this thread has used, in nanoseconds, or none where there is no way to ask.
///
/// Per thread and not per process. A pipeline instance runs on one thread, so the difference
/// between two of these readings around a call is what that call cost, whatever the other
/// thirty one threads were doing at the time.
#[must_use]
pub fn thread_cpu_ns() -> Option<u64> {
    #[cfg(all(
        target_pointer_width = "64",
        any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")
    ))]
    {
        let mut spent = system::Timespec { seconds: 0, nanoseconds: 0 };
        // SAFETY: `clock_gettime` writes two integers into the `timespec` it is given and reads
        // nothing else. The pointer is to a live local of exactly that layout, which the `repr(C)`
        // and the target conditions on this module make true, and it is not held after the call
        // returns.
        let worked = unsafe { system::clock_gettime(THREAD_CPUTIME, &raw mut spent) } == 0;
        if !worked {
            return None;
        }
        let seconds = u64::try_from(spent.seconds).ok()?;
        let nanoseconds = u64::try_from(spent.nanoseconds).ok()?;
        Some(seconds.saturating_mul(1_000_000_000).saturating_add(nanoseconds))
    }
    #[cfg(not(all(
        target_pointer_width = "64",
        any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")
    )))]
    {
        None
    }
}

/// One measured call, from the moment it started.
///
/// Both clocks are read when this is made and again when it is stopped, which is twice per operator
/// per chunk. That is the granularity rule: a clock read is a seam crossing and a seam crossing
/// happens once per chunk and never once per row.
#[derive(Debug)]
pub struct Span {
    wall: Instant,
    cpu: Option<u64>,
}

impl Span {
    /// Starts timing.
    #[must_use]
    pub fn start() -> Self {
        Self { wall: Instant::now(), cpu: thread_cpu_ns() }
    }

    /// Stops timing, and reports the wall nanoseconds and the CPU nanoseconds it took.
    ///
    /// The CPU number is zero on a platform with no thread clock. A document whose operators
    /// account for none of its CPU time says so in its warnings, which is the honest outcome and is
    /// better than a wall time reported twice under two names.
    #[must_use]
    pub fn stop(self) -> (u64, u64) {
        let wall = u64::try_from(self.wall.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let cpu = match (self.cpu, thread_cpu_ns()) {
            (Some(started), Some(ended)) => ended.saturating_sub(started),
            _ => 0,
        };
        (wall, cpu)
    }
}

#[cfg(test)]
mod tests {
    use super::{Span, thread_cpu_ns};

    #[test]
    fn the_thread_clock_does_not_go_backwards() {
        let Some(first) = thread_cpu_ns() else { return };
        let second = thread_cpu_ns().expect("a clock that answered once answers twice");
        assert!(second >= first, "{second} is before {first}");
    }

    #[test]
    fn a_span_over_work_costs_wall_time_and_cpu_time() {
        let span = Span::start();
        let mut counted: u64 = 0;
        for at in 0..2_000_000u64 {
            counted = counted.wrapping_add(at * at);
        }
        assert!(counted > 0, "the loop has to be kept");
        let (wall, cpu) = span.stop();
        assert!(wall > 0, "two million multiplications take longer than nothing");
        if thread_cpu_ns().is_some() {
            assert!(cpu > 0, "work on this thread costs this thread CPU time");
        }
    }
}
