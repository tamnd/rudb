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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rudb_common::{Cancel, Error, Result};
use rudb_metrics::Span;

use crate::pipeline::Pipeline;
use crate::pool::Lease;
use crate::serial::{Stop, instance, run_serial};

/// Run a pipeline on the threads the lease covers and combine what they produced.
///
/// The caller's thread is one of them, so a lease of one is exactly the serial driver and is handed
/// to it rather than being a special case in here. The rest are workers the pool already has parked,
/// and [`Lease::scatter`] is what wakes them and what makes sure this does not return until they
/// have all put down the pipeline they borrowed.
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
/// The first error any instance reported. The rest are dropped, because a query answers with one
/// error and the useful one is the one that happened first.
pub fn run_parallel(pipeline: &Pipeline<'_>, cancel: &Cancel, lease: &Lease<'_>) -> Result<u64> {
    let degree = lease.degree();
    if degree <= 1 {
        run_serial(pipeline, cancel)?;
        return Ok(0);
    }

    let stop = Stop::default();
    let failed = AtomicBool::new(false);
    let spent = AtomicU64::new(0);
    let failure = Mutex::new(None);

    // What a borrowed worker runs. It is the caller's own instance with a clock around it, because
    // the CPU clock this engine reads is per thread and the caller cannot see a worker's.
    let task = || {
        let measured = Span::start();
        let ran = one(pipeline, cancel, &stop, &failed);
        let (_, cpu) = measured.stop();
        spent.fetch_add(cpu, Ordering::Relaxed);
        keep(&failure, ran);
    };
    let (mine, panicked) = lease.scatter(&task, || one(pipeline, cancel, &stop, &failed));
    keep(&failure, mine);
    if panicked {
        keep(&failure, Err(panicked_thread()));
    }

    if let Some(error) = failure.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        return Err(error);
    }

    pipeline.sink().finalize_state()?;
    Ok(spent.load(Ordering::Relaxed))
}

/// Keeps the first error and drops the rest.
///
/// A query answers with one error and the useful one is the one that happened first. An instance
/// that fails asks the others to stop, so a second error is usually the same failure seen from
/// another thread.
fn keep(failure: &Mutex<Option<Error>>, ran: Result<()>) {
    if let Err(error) = ran {
        let mut held = failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.is_none() {
            *held = Some(error);
        }
    }
}

/// One instance, with its own local state, asking the others to stop if it fails.
///
/// The combine happens here, on the thread that filled the state, rather than back in the caller
/// once every thread has joined. What that buys is the merge: a hash aggregate combining thirty two
/// instances one after another on one thread is thirty two table merges in a row, and on a group by
/// with several hundred thousand groups that is most of the query. Combining where the state was
/// built lets an operator put two instances together while a third pair is being put together
/// beside it, and the aggregate does exactly that.
///
/// It is safe for the same reason [`Sink::combine`](crate::Sink::combine) taking its state by value
/// is safe. A local state belongs to one instance and nothing else reads it, so combining one while
/// another is still being filled touches nothing the other thread can see. What the combines share
/// is the operator's own global state, and an operator that said it would run as more than one
/// instance already has to guard that.
///
/// An instance that fails does not combine, and neither does one that finishes after another
/// instance has already failed. That is what `failed` is for, and it is a second flag rather than
/// the stop flag because stopping is not always a failure: a sink that has seen everything it wants
/// asks the others to stop too, and those instances still have to hand over what they built. Not
/// combining after a failure is partly to save the work, since a merge of two large tables is not
/// cheap and the query is already over, and partly so that the error a caller gets is the one that
/// ended the query rather than whatever a half filled state ran into on the way out.
///
/// An instance that fails while combining reports it the same way. The others may have combined
/// already and that is fine, because the query answers with the error and nothing reads what they
/// built. `finalize` is what turns a sink's state into an answer and it is not called at all when
/// anything failed.
fn one(pipeline: &Pipeline<'_>, cancel: &Cancel, stop: &Stop, failed: &AtomicBool) -> Result<()> {
    let mut locals = pipeline.locals();
    let ran = match instance(pipeline, cancel, stop, &mut locals) {
        Ok(()) if failed.load(Ordering::Relaxed) => return Ok(()),
        Ok(()) => pipeline.sink().combine_state(locals.sink),
        Err(error) => Err(error),
    };
    if let Err(error) = ran {
        failed.store(true, Ordering::Relaxed);
        stop.ask();
        return Err(error);
    }
    Ok(())
}

/// What a thread that panicked is reported as.
///
/// A panic in an operator is a bug in this engine rather than anything a query can cause, and the
/// thread it happened on has already printed it. What is left to do is fail the query rather than
/// let the other instances combine into a state that is missing whatever that one was holding.
fn panicked_thread() -> Error {
    Error::internal("a thread running part of this query panicked")
}
