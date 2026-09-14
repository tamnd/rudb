//! The single threaded driver, and the instance loop both drivers share.
//!
//! Written out in full because the claim that a push interface costs nothing on one thread should
//! be checkable by reading rather than believed. [`instance`] is the whole of it. The parallel
//! driver in `parallel` runs the same function on several threads and does the combining
//! afterwards, so what is written here is what a thread does either way and there is no second copy
//! of it to get wrong.

use std::sync::atomic::{AtomicBool, Ordering};

use rudb_common::{Cancel, Error, Result};
use rudb_vector::Chunk;

use crate::pipeline::{Locals, Pipeline};
use crate::progress::{Blocked, Progress};

/// Set when an instance has had everything it wants, so that the others stop taking morsels.
///
/// A `LIMIT` that has filled up and an operator that failed both mean the same thing to every other
/// instance, which is that there is no point reading more. On one thread it is the `break` that was
/// already there wearing a flag, and the read is once per morsel rather than once per chunk, so it
/// costs nothing on the path it did not use to be on.
#[derive(Debug, Default)]
pub(crate) struct Stop(AtomicBool);

impl Stop {
    /// Whether somebody has said to stop.
    pub(crate) fn asked(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Say so.
    pub(crate) fn ask(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Run a pipeline to completion on this thread.
///
/// One instance, one morsel at a time, in the order the source hands them out.
///
/// # Errors
///
/// Whatever any operator reports. Also an error if an operator returns
/// [`Progress::Blocked`], because parking a task needs a scheduler to run something else
/// meanwhile and the driver here has one thread and nothing to switch to. Nothing in the tree
/// returns `Blocked` yet.
pub fn run_serial(pipeline: &Pipeline<'_>, cancel: &Cancel) -> Result<()> {
    let stop = Stop::default();
    let mut locals = pipeline.locals();
    instance(pipeline, cancel, &stop, &mut locals)?;
    pipeline.sink().combine_state(locals.sink)?;
    pipeline.sink().finalize_state()
}

/// One instance of a pipeline, reading morsels until there are none left or somebody says stop.
///
/// It does not combine and it does not finalise, because with several instances those happen once
/// after all of them have finished rather than once each. The state it filled is left in `locals`
/// for whoever called it to hand over.
pub(crate) fn instance(
    pipeline: &Pipeline<'_>,
    cancel: &Cancel,
    stop: &Stop,
    locals: &mut Locals,
) -> Result<()> {
    let mut chunk = Chunk::empty(&[]);

    'morsels: while let Some(mut morsel) = taken(pipeline, stop) {
        // Before the first read of it rather than after the last, because a sink that puts chunks
        // back in the order the morsels were cut has to know where a chunk came from at the moment
        // it arrives, not once the morsel it came from is finished with.
        pipeline.sink().at_state(&morsel, &mut locals.sink)?;
        loop {
            cancel.check()?;

            let progress = match pipeline.source().read(&mut morsel, &mut chunk)? {
                Progress::Blocked(blocked) => return Err(parked(pipeline, blocked)),
                progress => progress,
            };

            let mut finished = false;
            // The operators that have more output in the chunk they were already given, which is a
            // cross product and nothing else today. Each one is asked again only after the chunk it
            // produced has been all the way through the sink, so there is one chunk in flight at a
            // time whatever any operator is in the middle of.
            let mut again: Vec<usize> = Vec::new();
            let mut from = 0;
            loop {
                for (at, (stream, local)) in
                    pipeline.streams().iter().zip(&mut locals.streams).enumerate().skip(from)
                {
                    match stream.push_state(&mut chunk, local)? {
                        Progress::Blocked(blocked) => return Err(parked(pipeline, blocked)),
                        Progress::Done => finished = true,
                        Progress::Again => again.push(at),
                        Progress::More => {}
                    }
                }

                match pipeline.sink().sink_state(&chunk, &mut locals.sink)? {
                    Progress::Blocked(blocked) => return Err(parked(pipeline, blocked)),
                    Progress::Done => finished = true,
                    // A sink produces nothing, so there is nothing for it to have more of.
                    Progress::More | Progress::Again => {}
                }

                // The last one to ask is the one nearest the sink, and it goes first because the
                // operators above it have already seen what it produced. Going the other way round
                // would hand them a second chunk built from an input they were half way through.
                match again.pop() {
                    Some(at) if !finished => from = at,
                    _ => break,
                }
            }

            if finished {
                stop.ask();
                break 'morsels;
            }
            if progress == Progress::Done {
                break;
            }
        }
    }

    Ok(())
}

/// The next morsel, or nothing once somebody has said to stop.
fn taken(pipeline: &Pipeline<'_>, stop: &Stop) -> Option<crate::morsel::Morsel> {
    if stop.asked() {
        return None;
    }
    pipeline.source().morsel()
}

/// The error a blocked operator gets on the serial driver.
///
/// It names the pipeline and the reason rather than saying that something is unimplemented,
/// because the useful half of the report is which of the four reasons it was.
fn parked(pipeline: &Pipeline<'_>, blocked: Blocked) -> Error {
    Error::not_implemented(format!(
        "{} blocked {} and the driver has nothing else to run, which is the scheduler's job",
        pipeline.id(),
        blocked
    ))
}
