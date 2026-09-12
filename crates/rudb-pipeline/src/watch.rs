//! The instrumentation shim: one wrapper that measures whatever it is put around.
//!
//! The point of it being a wrapper rather than a call inside each operator is that an operator
//! cannot forget. Nothing in a source, a stream or a sink mentions a clock or a counter, and the
//! code that builds a pipeline puts one of these around every operator it builds, so a new operator
//! is measured the day it is written by somebody who never read this file.
//!
//! It measures per call, which is per chunk, which is the granularity rule. Two clock readings
//! around a call that handles two thousand rows is not a measurement anybody can feel. Two clock
//! readings per row would be the measurement rather than the thing measured.
//!
//! What it does not do is count bytes or memory. A wrapper cannot see a read or a reservation, it
//! can only see chunks going past, so [`Counters::read`], [`Counters::decoded`],
//! [`Counters::spilled`] and [`Counters::holding`] are reported by whoever does those things,
//! which is the file readers and the buffer manager.

use std::sync::Arc;

use rudb_common::Result;
use rudb_metrics::{Counters, Span};
use rudb_vector::Chunk;

use crate::morsel::Morsel;
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

    fn read(&self, morsel: &mut Morsel, out: &mut Chunk) -> Result<Progress> {
        let span = Span::start();
        let progress = self.inner.read(morsel, out);
        let (wall, cpu) = span.stop();
        self.counters.spent(wall, cpu);
        // A failed read still cost the time it took, which is why the time is recorded above
        // whatever happened. The rows are only there to count if the call produced any.
        if progress.is_ok() {
            self.counters.made(rows(out));
        }
        progress
    }
}

impl<S: Stream> Stream for Watched<S> {
    type Local = S::Local;

    fn local(&self) -> Self::Local {
        self.inner.local()
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Self::Local) -> Result<Progress> {
        // A stream transforms in place, so the rows it was given have to be counted before the call
        // and the rows it produced after it. A filter that keeps a tenth of its input is the
        // difference between those two numbers and nothing else records it.
        let taken = rows(chunk);
        let span = Span::start();
        let progress = self.inner.push(chunk, local);
        let (wall, cpu) = span.stop();
        self.counters.spent(wall, cpu);
        if progress.is_ok() {
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

    fn sink(&self, chunk: &Chunk, local: &mut Self::Local) -> Result<Progress> {
        let taken = rows(chunk);
        let span = Span::start();
        let progress = self.inner.sink(chunk, local);
        let (wall, cpu) = span.stop();
        self.counters.spent(wall, cpu);
        if progress.is_ok() {
            self.counters.took(taken);
        }
        progress
    }

    /// Measured, because merging one thread's state into the global one is work and on an aggregate
    /// it is a lot of it.
    fn combine(&self, local: Self::Local) -> Result<()> {
        let span = Span::start();
        let combined = self.inner.combine(local);
        let (wall, cpu) = span.stop();
        self.counters.spent(wall, cpu);
        combined
    }

    /// Measured, and this is the one that matters most. A sort does its sorting here, so an
    /// operator whose `sink` calls look free and whose `finalize` takes a second is the shape the
    /// document has to show rather than hide.
    ///
    /// The rows a sink produced are not counted here. They come out of whatever source reads its
    /// finished state, and that source is measured in its own right, so counting them in both
    /// places would put the same rows in the document twice.
    fn finalize(&self) -> Result<()> {
        let span = Span::start();
        let finished = self.inner.finalize();
        let (wall, cpu) = span.stop();
        self.counters.spent(wall, cpu);
        finished
    }
}

/// The rows in a chunk, as the counters want them.
fn rows(chunk: &Chunk) -> u64 {
    u64::try_from(chunk.len()).unwrap_or(u64::MAX)
}
