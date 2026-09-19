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

use crate::morsel::Morsel;
use crate::pipeline::{Locals, Pipeline};
use crate::pool::Lease;
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
    let alone = Lease::alone();
    for stream in pipeline.streams() {
        stream.prepare_once(&alone)?;
    }
    pipeline.sink().prepare_once(&alone)?;
    let mut locals = pipeline.locals();
    instance(pipeline, cancel, &stop, &mut locals)?;
    pipeline.sink().combine_state(locals.sink)?;
    drain(pipeline, cancel)?;
    pipeline.sink().finalize_state(&alone)
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

            if through(pipeline, 0, &mut chunk, locals)? == Progress::Done {
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

/// Push one chunk through the operators from `start` downwards and into the sink.
///
/// [`Progress::Done`] means somebody below wanted no more input at all, which a caller reading
/// morsels should take as leave to stop reading them. The chunk that came with it has already been
/// delivered, which is what [`Stream::push`](crate::Stream::push) promises.
///
/// `start` is where to begin, and it is not always zero. A chunk that came out of a drain enters
/// the pipeline below the operator that drained it rather than at the top, because the operators
/// above that one have already seen everything they were going to see.
pub(crate) fn through(
    pipeline: &Pipeline<'_>,
    start: usize,
    chunk: &mut Chunk,
    locals: &mut Locals,
) -> Result<Progress> {
    let mut finished = false;
    // The operators that have more output in the chunk they were already given, which is a cross
    // product and nothing else today. Each one is asked again only after the chunk it produced has
    // been all the way through the sink, so there is one chunk in flight at a time whatever any
    // operator is in the middle of.
    let mut again: Vec<usize> = Vec::new();
    let mut from = start;
    loop {
        for (at, (stream, local)) in
            pipeline.streams().iter().zip(&mut locals.streams).enumerate().skip(from)
        {
            match stream.push_state(chunk, local)? {
                Progress::Blocked(blocked) => return Err(parked(pipeline, blocked)),
                Progress::Done => finished = true,
                Progress::Again => again.push(at),
                Progress::More => {}
            }
        }

        match pipeline.sink().sink_state(chunk, &mut locals.sink)? {
            Progress::Blocked(blocked) => return Err(parked(pipeline, blocked)),
            Progress::Done => finished = true,
            // A sink produces nothing, so there is nothing for it to have more of.
            Progress::More | Progress::Again => {}
        }

        // The last one to ask is the one nearest the sink, and it goes first because the operators
        // above it have already seen what it produced. Going the other way round would hand them a
        // second chunk built from an input they were half way through.
        match again.pop() {
            Some(at) if !finished => from = at,
            _ => break,
        }
    }
    Ok(if finished { Progress::Done } else { Progress::More })
}

/// Take from every operator that owes chunks, once every instance has finished reading.
///
/// One thread, after the last instance and before the sink is finalised. It is a separate pass
/// rather than something the last instance out does, because what an operator owes at the end is
/// usually a fact about every instance put together and no instance knows it is the last.
///
/// The state it fills is a fresh set of locals combined into the sink like any other instance's,
/// which is what makes this additive: the instances that already ran have already handed theirs
/// over, and a sink that can merge two of them can merge three. Nothing is allocated at all on a
/// pipeline where no operator owes anything, which is every pipeline but the ones ending in an
/// outer join that gathered the side it keeps.
///
/// Top down, because an operator's drain goes to whatever is below it and an operator below may
/// owe chunks of its own once it has seen them.
pub(crate) fn drain(pipeline: &Pipeline<'_>, cancel: &Cancel) -> Result<()> {
    if !pipeline.streams().iter().any(|stream| stream.drains_once()) {
        return Ok(());
    }
    let mut locals = pipeline.locals();
    // A drained chunk came from no morsel, and a root putting chunks back into the order the
    // morsels were cut in needs somewhere to put it. After the last of them is where it goes, and
    // that is not a choice: these are the rows an operator could only produce once every morsel
    // had been read. A morsel number no source will ever hand out says so, and a root that is not
    // restoring any order ignores this the way it ignores every other morsel.
    pipeline.sink().at_state(&Morsel::new(u64::MAX, 0, 0), &mut locals.sink)?;
    for at in 0..pipeline.streams().len() {
        if !pipeline.streams()[at].drains_once() {
            continue;
        }
        let taking = &mut locals;
        pipeline.streams()[at].drain_once(&mut |chunk| {
            cancel.check()?;
            through(pipeline, at + 1, chunk, taking)
        })?;
    }
    pipeline.sink().combine_state(locals.sink)
}

/// The next morsel, or nothing once somebody has said to stop.
fn taken(pipeline: &Pipeline<'_>, stop: &Stop) -> Option<Morsel> {
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
