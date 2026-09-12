//! The single threaded driver.
//!
//! Written out in full because the claim that a push interface costs nothing on one thread should
//! be checkable by reading rather than believed. This is the whole of it. The parallel driver is
//! F4, it is several times longer, and nothing above the driver changes between the two.

use rudb_common::{Cancel, Error, Result};
use rudb_vector::Chunk;

use crate::pipeline::Pipeline;
use crate::progress::{Blocked, Progress};

/// Run a pipeline to completion on this thread.
///
/// One instance, one morsel at a time, in the order the source hands them out.
///
/// # Errors
///
/// Whatever any operator reports. Also an error if an operator returns
/// [`Progress::Blocked`], because parking a task needs a scheduler to run something else
/// meanwhile and F0 has one thread and nothing to switch to. Nothing in the tree returns `Blocked`
/// yet. F4 replaces that arm with a park and this function stops being the one that runs.
pub fn run_serial(pipeline: &Pipeline, cancel: &Cancel) -> Result<()> {
    let mut locals = pipeline.locals();
    let mut chunk = Chunk::empty(&[]);

    'morsels: while let Some(mut morsel) = pipeline.source().morsel() {
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
                break 'morsels;
            }
            if progress == Progress::Done {
                break;
            }
        }
    }

    pipeline.sink().combine_state(locals.sink)?;
    pipeline.sink().finalize_state()
}

/// The error a blocked operator gets on the serial driver.
///
/// It names the pipeline and the reason rather than saying that something is unimplemented,
/// because the useful half of the report is which of the four reasons it was.
fn parked(pipeline: &Pipeline, blocked: Blocked) -> Error {
    Error::not_implemented(format!(
        "{} blocked {} and the serial driver has nothing else to run, which is F4",
        pipeline.id(),
        blocked
    ))
}
