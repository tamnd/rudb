//! The instrumentation shim: one wrapper that measures whatever it is put around.
//!
//! The point of it being a wrapper rather than a call inside each operator is that an operator
//! cannot forget. Nothing in a source, a stream or a sink mentions a clock or a counter, and the
//! code that builds a pipeline puts one of these around every operator it builds, so a new operator
//! is measured the day it is written by somebody who never read this file.
//!
//! It measures per call, which is per chunk, which is the granularity rule. Two clock readings
//! around a call that handles a thousand rows is not a measurement anybody can feel. Two clock
//! readings per row would be the measurement rather than the thing measured.
//!
//! That holds for the wall clock, which Linux answers out of the vDSO without entering the kernel.
//! It does not hold for the thread CPU clock, which is a real system call every time and costs more
//! per chunk than a chunk of a thousand rows costs to produce: a count over twenty million rows was
//! fourteen times slower with it than without. So the thread clock is read here only when the
//! operator was built saying its row wants a CPU column, which is what `EXPLAIN ANALYZE` and
//! `enable_profiling` arrange. Every query still gets wall time per operator, and CPU time per
//! pipeline and per worker, because those spans are taken once per pipeline rather than once per
//! chunk.
//!
//! It also reads the slow path counter on either side of the call, and the difference is what that
//! operator gave up on inside that chunk. That is the whole reason the counter is per thread: a
//! difference around a call only means something if nothing else was counting into it at the time.
//! The stage clock is read the same way and for the same reason, and the difference is where a scan
//! spent that chunk: reading, decompressing, decoding, building a dictionary or assembling.
//!
//! What it does not do is count bytes or memory. A wrapper cannot see a read or a reservation, it
//! can only see chunks going past, so [`Counters::read`], [`Counters::decoded`],
//! [`Counters::spilled`] and [`Counters::holding`] are reported by whoever does those things,
//! which is the file readers and the buffer manager.

use std::sync::Arc;

use rudb_common::{Result, Spent, Tally, slow, stage};
use rudb_metrics::{Counters, Span};
use rudb_vector::Chunk;

use crate::morsel::Morsel;
use crate::pool::Lease;
use crate::progress::Progress;
use crate::traits::{Sink, Source, Stream};

/// One operator with a clock and a set of counters around it.
#[derive(Debug)]
pub struct Watched<T> {
    inner: T,
    counters: Arc<Counters>,
}

impl<T> Watched<T> {
    /// Wraps `inner`, counting into `counters`.
    ///
    /// The counters are shared rather than owned because every thread running this pipeline calls
    /// this same object, and because whoever built the pipeline keeps a handle to read the numbers
    /// out of at the end.
    pub fn new(inner: T, counters: Arc<Counters>) -> Self {
        Self { inner, counters }
    }

    /// What this wrapper counts into.
    pub fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }
}

impl<S: Source> Source for Watched<S> {
    /// Not measured. Handing out a morsel is a number in an atomic, it is called once per unit of
    /// work rather than once per chunk, and a clock reading around it would cost more than it.
    fn morsel(&self) -> Option<Morsel> {
        self.inner.morsel()
    }

    fn morsels(&self, threads: usize, weight: usize) -> Option<usize> {
        self.inner.morsels(threads, weight)
    }

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let measure = Measure::start(&self.counters);
        let progress = self.inner.read(morsel, out);
        measure.stop(&self.counters);
        // A failed read still cost the time it took, which is why the time is recorded above
        // whatever happened. The rows are only there to count if the call produced any.
        if progress.is_ok() {
            self.counters.made(rows(out));
        }
        progress
    }

    /// Passed through, because the wrapper is not the thing that knows what a row costs. The same
    /// trap as the two `weight` methods below, and caught the same way.
    fn weight(&self) -> usize {
        self.inner.weight()
    }
}

/// One instance of a watched stream, and whether the chunk it is handed next is input.
///
/// An operator that answers [`Progress::Again`] is called back with the chunk it just produced, once
/// everything below it has had that chunk, so what is in it on the next call is what is left of its
/// own output rather than rows anybody gave it. Counting those charges the operator for a share of
/// its own answer as if it were input, and the input count is the one number a reader uses to check
/// an operator against what the operators under it produced.
#[derive(Debug)]
pub struct Resumed<L> {
    inner: L,
    /// Whether the last call said it had more output in the input it already had.
    resuming: bool,
}

impl<S: Stream> Stream for Watched<S> {
    type Local = Resumed<S::Local>;

    fn local(&self) -> Self::Local {
        Resumed { inner: self.inner.local(), resuming: false }
    }

    fn parallel(&self) -> bool {
        self.inner.parallel()
    }

    /// Passed through, for the same reason `finalize_degree` below is.
    fn weight(&self) -> usize {
        self.inner.weight()
    }

    /// Counted against this operator the way its pushes are, because it is its work.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let prepared = self.inner.prepare(threads);
        measure.stop(&self.counters);
        prepared
    }

    fn drains(&self) -> bool {
        self.inner.drains()
    }

    /// Measured, and the rows counted, because a drain is chunks this operator produced and the
    /// document would otherwise show an operator that made fewer rows than it handed on.
    ///
    /// The clock covers the callback as well as the operator, which charges this operator for
    /// whatever sits below it. There is nowhere better to put that time: the operators below are
    /// measured by their own shims when the callback reaches them, so what is left here is theirs
    /// counted twice rather than a gap, and the alternative is a clock started and stopped around
    /// every chunk of a drain that is one chunk long on almost every query.
    fn drain(&self, out: &mut dyn FnMut(&mut Chunk) -> Result<Progress>) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let drained = self.inner.drain(&mut |chunk| {
            self.counters.made(rows(chunk));
            out(chunk)
        });
        measure.stop(&self.counters);
        drained
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Self::Local) -> Result<Progress> {
        // A stream transforms in place, so the rows it was given have to be counted before the call
        // and the rows it produced after it. A filter that keeps a tenth of its input is the
        // difference between those two numbers and nothing else records it.
        //
        // Nothing to count on a call that carries on where the last one stopped. See [`Resumed`].
        let taken = if local.resuming { 0 } else { rows(chunk) };
        let measure = Measure::start(&self.counters);
        let progress = self.inner.push(chunk, &mut local.inner);
        measure.stop(&self.counters);
        if progress.is_ok() {
            local.resuming = matches!(progress, Ok(Progress::Again));
            self.counters.took(taken);
            self.counters.made(rows(chunk));
        }
        progress
    }
}

impl<K: Sink> Sink for Watched<K> {
    type Local = K::Local;

    fn local(&self) -> Self::Local {
        self.inner.local()
    }

    fn parallel(&self) -> bool {
        self.inner.parallel()
    }

    /// Passed through, because the wrapper is not the thing that knows what a row costs.
    ///
    /// The same trap as `finalize_degree` below, and it caught the same way. Every operator the
    /// engine builds is wrapped in one of these, so a weight that stops here is a weight nothing
    /// ever reads, and the default it falls back to is zero, which says a grouped aggregate over
    /// five columns costs a row no more than a projection that renames one.
    fn weight(&self) -> usize {
        self.inner.weight()
    }

    /// Passed through, because the wrapper is not the thing that knows how wide the finish is.
    ///
    /// Everything in the engine is wrapped in one of these, so a question that stops here is a
    /// question that is never asked. This one stopped here, and the default it fell back to is one,
    /// so every pipeline borrowed as many threads as its source had morsels and finished on those
    /// however many the sink said it could use. On a thirty two thread machine `COUNT(DISTINCT
    /// UserID) GROUP BY RegionID` over a million rows cut sixteen morsels and then deduplicated nine
    /// hundred thousand pairs on sixteen threads with the other half of the machine parked.
    fn finalize_degree(&self, ceiling: usize) -> usize {
        self.inner.finalize_degree(ceiling)
    }

    /// Counted against this operator the way its chunks are, because it is its work.
    fn prepare(&self, threads: &Lease<'_>) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let prepared = self.inner.prepare(threads);
        measure.stop(&self.counters);
        prepared
    }

    /// Measured like the rest, even though it moves no rows. It takes a lock on the ordered root and
    /// a lock that turns out to be contended is exactly the sort of thing this wrapper exists to
    /// show rather than leave somebody to guess at.
    fn at(&self, morsel: &Morsel, local: &mut Self::Local) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let noted = self.inner.at(morsel, local);
        measure.stop(&self.counters);
        noted
    }

    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress> {
        let taken = rows(chunk);
        let measure = Measure::start(&self.counters);
        let progress = self.inner.sink(chunk, local);
        measure.stop(&self.counters);
        if progress.is_ok() {
            self.counters.took(taken);
        }
        progress
    }

    /// Measured, because merging one thread's state into the global one is work and on an aggregate
    /// it is a lot of it.
    fn combine(&self, local: Self::Local) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let combined = self.inner.combine(local);
        measure.stop(&self.counters);
        combined
    }

    /// Measured, and this is the one that matters most. A sort does its sorting here, so an
    /// operator whose `sink` calls look free and whose `finalize` takes a second is the shape the
    /// document has to show rather than hide.
    ///
    /// The rows a sink produced are not counted here. They come out of whatever source reads its
    /// finished state, and that source is measured in its own right, so counting them in both
    /// places would put the same rows in the document twice.
    fn finalize(&self, threads: &Lease<'_>) -> Result<()> {
        let measure = Measure::start(&self.counters);
        let finished = self.inner.finalize(threads);
        measure.stop(&self.counters);
        finished
    }
}

/// A clock and a slow path reading, started together and reported together.
///
/// One type rather than two pairs of lines in five methods, because the two are always taken at the
/// same two moments and a method that started one and forgot the other would be a method whose
/// operator looks like it never falls back.
struct Measure {
    span: Span,
    before: Tally,
    reading: Spent,
}

impl Measure {
    /// Reads both, with the clock last so that as little as possible sits between it and the call.
    ///
    /// The clock is the wall clock unless `counters` says this operator's row wants a CPU column.
    /// Reading the thread clock is a system call and this runs twice per operator per chunk, so on a
    /// chunk of a thousand rows it costs more than the call it is timing. See
    /// [`Span`](rudb_metrics::Span).
    fn start(counters: &Counters) -> Self {
        let before = slow::here();
        let reading = stage::here();
        Self { span: Span::charging(counters.charges_cpu()), before, reading }
    }

    /// Reads both again and charges the difference to the operator.
    ///
    /// The time is charged whatever happened, because a call that failed still cost what it took.
    /// So is the falling back, for the same reason.
    fn stop(self, counters: &Counters) {
        let (wall, cpu) = self.span.stop();
        counters.spent(wall, cpu);
        counters.fell_back(slow::here().since(self.before));
        counters.spent_reading(stage::here().since(self.reading));
    }
}

/// The rows in a chunk, as the counters want them.
fn rows(chunk: &Chunk) -> u64 {
    u64::try_from(chunk.len()).unwrap_or(u64::MAX)
}
