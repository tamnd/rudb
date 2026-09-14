//! The driver that runs one pipeline on several threads.
//!
//! It is the serial driver with the instance loop run more than once. That is not a simplification
//! for the doc comment, it is the file: [`instance`] is shared with `serial`, and what is written
//! here is the starting, the collecting and the combining around it.
//!
//! # What makes it safe to run twice
//!
//! Three things, and all three were decided at F0 rather than here. A source is one object that
//! every instance calls, so handing out work is an atomic on a counter and not a queue somebody has
//! to own. A stream and a sink take `&self` with their mutable state passed in, so N instances are
//! N local states and one operator. And [`Sink::combine`](crate::Sink::combine) takes that state by
//! value, so putting one instance's work into the global state consumes it and cannot happen twice.
//!
//! # What is not here yet
//!
//! Work stealing, which needs a queue rather than a counter. Parking, because nothing returns
//! [`Progress::Blocked`](crate::Progress::Blocked) yet and the arm that would park is still the arm
//! that reports. And a pipeline running while another pipeline of the same query runs, since
//! [`Query::run`](https://docs.rs/rudb-exec) still takes them one at a time and the dependency edges
//! are what would decide which may overlap.

use std::sync::atomic::{AtomicU64, Ordering};

use rudb_common::{Cancel, Error, Result};
use rudb_metrics::Span;

use crate::pipeline::{Locals, Pipeline};
use crate::serial::{Stop, instance, run_serial};

/// Run a pipeline on `degree` threads and combine what they produced.
///
/// The caller's thread is one of them, so `degree` of one is exactly the serial driver and is
/// handed to it rather than being a special case in here.
///
/// Returns the CPU nanoseconds burned on the threads that were not the caller's. The clock this
/// engine reads for CPU time is per thread, which is the right clock for attributing work to an
/// operator and the wrong one for a span that wants the whole query, so a caller timing the
/// execution has to be told what it could not see. Wall time is not reported for the same reason in
/// reverse: a worker ran at the same time as the caller, and adding its wall clock would make a
/// query that got faster look like it took longer.
///
/// # Errors
///
/// The first error any instance reported. The rest are dropped rather than collected, because a
/// query answers with one error and the useful one is the one that happened first. An instance that
/// fails asks the others to stop, so a second error is usually the same failure seen from another
/// thread.
pub fn run_parallel(pipeline: &Pipeline<'_>, cancel: &Cancel, degree: usize) -> Result<u64> {
    if degree <= 1 {
        run_serial(pipeline, cancel)?;
        return Ok(0);
    }

    let stop = Stop::default();
    let spent = AtomicU64::new(0);
    let mut done: Vec<Result<Locals>> = Vec::with_capacity(degree);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(degree - 1);
        for _ in 1..degree {
            handles.push(scope.spawn(|| {
                let measured = Span::start();
                let ran = one(pipeline, cancel, &stop);
                let (_, cpu) = measured.stop();
                spent.fetch_add(cpu, Ordering::Relaxed);
                ran
            }));
        }
        // The thread that asked runs an instance too, rather than waiting on the ones it started.
        // A degree of two that keeps one thread idle is not a degree of two.
        done.push(one(pipeline, cancel, &stop));
        for handle in handles {
            done.push(handle.join().unwrap_or_else(|_| Err(panicked())));
        }
    });

    let mut locals = Vec::with_capacity(done.len());
    let mut failure = None;
    for finished in done {
        match finished {
            Ok(local) => locals.push(local),
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }

    // After every instance, never during. An operator merging a second instance's state while a
    // third is still filling its own would be reading half of an answer, and the combine is where
    // a hash aggregate does the work that makes a wrong `SUM` hard to notice.
    for local in locals {
        pipeline.sink().combine_state(local.sink)?;
    }
    pipeline.sink().finalize_state()?;
    Ok(spent.load(Ordering::Relaxed))
}

/// One instance, with its own local state, asking the others to stop if it fails.
fn one(pipeline: &Pipeline<'_>, cancel: &Cancel, stop: &Stop) -> Result<Locals> {
    let mut locals = pipeline.locals();
    match instance(pipeline, cancel, stop, &mut locals) {
        Ok(()) => Ok(locals),
        Err(error) => {
            stop.ask();
            Err(error)
        }
    }
}

/// What a thread that panicked is reported as.
///
/// A panic in an operator is a bug in this engine rather than anything a query can cause, and the
/// thread it happened on has already printed it. What is left to do is fail the query rather than
/// let the other instances combine into a state that is missing whatever that one was holding.
fn panicked() -> Error {
    Error::internal("a thread running part of this query panicked")
}
