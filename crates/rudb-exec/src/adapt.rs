//! Driving a push operator from the pull tree, until there is no pull tree left.
//!
//! The operators moved behind the push traits one at a time rather than in one commit, which meant
//! there was a period where a [`Stream`] had to sit in a tree whose other nodes still had
//! [`Operator::next`]. This is the one place that knows how to do that. It pulls a chunk from
//! below, pushes it through the stream, and hands back whatever came out.
//!
//! Every operator has moved now, so what is left of the pull tree is these adapters and the
//! [`Operator`] trait they are written against. The shape of the tree is still a tree: a node owns
//! its children and the root is pulled from. Turning that into a list of pipelines the driver runs
//! is the next thing, and it deletes this file rather than changing it.
//!
//! What survives is the [`Source`], [`Stream`] and [`Sink`] implementations, which is the point of
//! moving them first.

use std::fmt;

use rudb_common::{Error, Result};
use rudb_pipeline::{Morsel, Progress, Sink, Source, Stream};
use rudb_vector::Chunk;

use crate::buffer::Buffered;
use crate::operator::Operator;
use crate::schema::Schema;

/// One source at the bottom of the pull tree.
///
/// A source hands out morsels and fills a chunk from one, and the tree above it asks for a chunk at
/// a time, so this is the loop between the two: take a morsel, read from it until it is drained,
/// take the next one, stop when there are none left. It is the same loop the serial driver runs, on
/// one morsel at a time rather than on the whole pipeline, which is what makes it a faithful
/// stand in until the driver is what runs the tree.
///
/// One morsel is in flight here because there is one thread here. Nothing about the source says so,
/// which is the point: the same object is what F4 hands to several threads at once.
pub(crate) struct Pulled<S: Source> {
    source: S,
    schema: Schema,
    /// The morsel being read, kept between calls because a morsel holds more than a chunk.
    morsel: Option<Morsel>,
    done: bool,
}

impl<S: Source> Pulled<S> {
    /// `schema` is what the source produces.
    pub(crate) fn new(source: S, schema: Schema) -> Self {
        Self { source, schema, morsel: None, done: false }
    }
}

impl<S: Source> fmt::Debug for Pulled<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pulled").field("source", &self.source).finish_non_exhaustive()
    }
}

impl<S: Source> Operator for Pulled<S> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        while !self.done {
            let Some(mut morsel) = self.morsel.take().or_else(|| self.source.morsel()) else {
                self.done = true;
                break;
            };
            let mut chunk = Chunk::empty(&[]);
            match self.source.read(&mut morsel, &mut chunk)? {
                Progress::Blocked(blocked) => return Err(parked(&blocked)),
                // Done is the call that drained the morsel, so the next one comes from the source.
                Progress::Done => {}
                _ => self.morsel = Some(morsel),
            }
            // An empty chunk is the end of a file or a morsel that held nothing, and handing it on
            // would be a call's worth of work at every level of the tree for no rows.
            if !chunk.is_empty() {
                return Ok(Some(chunk));
            }
        }
        Ok(None)
    }
}

/// One streaming operator with the tree below it.
pub(crate) struct Streamed<'a, S: Stream> {
    input: Box<dyn Operator + 'a>,
    stream: S,
    local: S::Local,
    schema: Schema,
    done: bool,
    /// Whether the stream has more output for the chunk it already has, which is what a cross
    /// product says once per right chunk.
    again: bool,
}

impl<'a, S: Stream> Streamed<'a, S> {
    /// `schema` is what this operator produces, which for a filter or a limit is the input's and
    /// for a projection is its own.
    pub(crate) fn new(input: Box<dyn Operator + 'a>, stream: S, schema: Schema) -> Self {
        let local = stream.local();
        Self { input, stream, local, schema, done: false, again: false }
    }
}

impl<S: Stream> fmt::Debug for Streamed<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Streamed")
            .field("stream", &self.stream)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

impl<S: Stream> Operator for Streamed<'_, S> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        while !self.done {
            // An operator that asked to be called again is holding its own input, and the contract
            // says it overwrites whatever it is handed, so there is nothing to pull for it.
            let mut chunk = if self.again {
                Chunk::empty(&[])
            } else {
                let Some(chunk) = self.input.next()? else { break };
                chunk
            };
            self.again = false;
            match self.stream.push(&mut chunk, &mut self.local)? {
                Progress::Done => self.done = true,
                Progress::Again => self.again = true,
                Progress::Blocked(blocked) => return Err(parked(&blocked)),
                // `Progress` is non exhaustive, and anything added to it later is something this
                // adapter has no idea what to do with, so it is treated as ordinary progress. The
                // pull tree is going away before that can matter.
                _ => {}
            }
            // An empty chunk is skipped rather than handed on, because every operator above would
            // do a call's worth of work for no rows. A stream that finished on an empty chunk is
            // finished either way, and the loop condition catches that on the way round.
            if !chunk.is_empty() {
                return Ok(Some(chunk));
            }
        }
        Ok(None)
    }
}

/// One streaming operator that cannot start until another pipeline has finished.
///
/// A cross product streams its left side and replays its right side, so the right side has to be in
/// hand before the first left chunk arrives. That is the same dependency edge [`Paired`] runs, with
/// a stream on the near end of it rather than a sink, so this runs the pipeline it depends on first
/// and then gets out of the way.
pub(crate) struct Fed<'a, F: Sink, S: Stream> {
    first: Box<dyn Operator + 'a>,
    aside: F,
    then: Streamed<'a, S>,
    built: bool,
}

impl<'a, F: Sink, S: Stream> Fed<'a, F, S> {
    /// `first` and `aside` are the side that has to be finished first, and `then` is the stream with
    /// the rest of the tree under it.
    pub(crate) fn new(first: Box<dyn Operator + 'a>, aside: F, then: Streamed<'a, S>) -> Self {
        Self { first, aside, then, built: false }
    }
}

impl<F: Sink, S: Stream> fmt::Debug for Fed<'_, F, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fed")
            .field("aside", &self.aside)
            .field("then", &self.then)
            .finish_non_exhaustive()
    }
}

impl<F: Sink, S: Stream> Operator for Fed<'_, F, S> {
    fn schema(&self) -> &Schema {
        self.then.schema()
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            drain(self.first.as_mut(), &self.aside)?;
            self.built = true;
        }
        self.then.next()
    }
}

/// One pipeline breaker with the tree below it.
///
/// A sink sees its whole input before it produces anything, so this drains the tree below into it
/// on the first call, combines the one instance there is, finalises, and then reads the finished
/// chunks back out of the [`Buffered`] the sink filled. The order is the order the serial driver
/// uses, which is on purpose: when the tree is built as a pipeline this adapter is deleted and the
/// driver does exactly this.
pub(crate) struct Broken<'a, K: Sink> {
    input: Box<dyn Operator + 'a>,
    sink: K,
    out: Buffered,
    schema: Schema,
    built: bool,
    at: usize,
}

impl<'a, K: Sink> Broken<'a, K> {
    /// `out` is the source half the sink finalises into, and `schema` is what comes out of it.
    pub(crate) fn new(
        input: Box<dyn Operator + 'a>,
        sink: K,
        out: Buffered,
        schema: Schema,
    ) -> Self {
        Self { input, sink, out, schema, built: false, at: 0 }
    }

    fn build(&mut self) -> Result<()> {
        drain(self.input.as_mut(), &self.sink)
    }
}

impl<K: Sink> fmt::Debug for Broken<'_, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Broken")
            .field("sink", &self.sink)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

impl<K: Sink> Operator for Broken<'_, K> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            self.build()?;
            self.built = true;
        }
        let chunk = self.out.at(self.at)?;
        if chunk.is_some() {
            self.at += 1;
        }
        Ok(chunk)
    }
}

/// Two pipeline breakers where the second one needs the first one's answer.
///
/// A set operation reads the whole right side before it can decide anything about a left row, and a
/// hash join builds from one side before it probes with the other. In push terms that is two
/// pipelines with a dependency edge between them, and the edge means one order: everything into
/// `aside`, and only then everything into `sink`. The scheduler is what enforces that later, from
/// the same edge, which is why this runs them in the order it does rather than in whatever order is
/// convenient here.
pub(crate) struct Paired<'a, F: Sink, K: Sink> {
    first: Box<dyn Operator + 'a>,
    aside: F,
    second: Box<dyn Operator + 'a>,
    sink: K,
    out: Buffered,
    schema: Schema,
    built: bool,
    at: usize,
}

impl<'a, F: Sink, K: Sink> Paired<'a, F, K> {
    /// `first` and `aside` are the side that has to be finished first, `second` and `sink` are the
    /// side that uses it, and `out` is what `sink` finalises into.
    pub(crate) fn new(
        first: Box<dyn Operator + 'a>,
        aside: F,
        second: Box<dyn Operator + 'a>,
        sink: K,
        out: Buffered,
        schema: Schema,
    ) -> Self {
        Self { first, aside, second, sink, out, schema, built: false, at: 0 }
    }

    fn build(&mut self) -> Result<()> {
        drain(self.first.as_mut(), &self.aside)?;
        drain(self.second.as_mut(), &self.sink)
    }
}

impl<F: Sink, K: Sink> fmt::Debug for Paired<'_, F, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Paired")
            .field("sink", &self.sink)
            .field("aside", &self.aside)
            .finish_non_exhaustive()
    }
}

impl<F: Sink, K: Sink> Operator for Paired<'_, F, K> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        if !self.built {
            self.build()?;
            self.built = true;
        }
        let chunk = self.out.at(self.at)?;
        if chunk.is_some() {
            self.at += 1;
        }
        Ok(chunk)
    }
}

/// Runs one whole pipeline into one sink, the way the serial driver will.
///
/// One instance, because there is one thread here. `combine` takes it by value and `finalize`
/// happens once after it, which is the contract every sink is written against, so the only thing
/// that changes when there are several threads is how many times the first three lines happen.
fn drain<K: Sink>(input: &mut dyn Operator, sink: &K) -> Result<()> {
    let mut local = sink.local();
    while let Some(chunk) = input.next()? {
        match sink.sink(&chunk, &mut local)? {
            Progress::Done => break,
            Progress::Blocked(blocked) => return Err(parked(&blocked)),
            _ => {}
        }
    }
    sink.combine(local)?;
    sink.finalize()
}

/// The error a blocked stream gets here.
///
/// Nothing in the tree returns [`Progress::Blocked`] yet, and the operators that will are the ones
/// that wait on io or on memory rather than the three streaming ones. It is an error rather than a
/// panic because a wrong answer is worse than a failed query, and it names the reason because that
/// is the useful half of the report.
fn parked(blocked: &rudb_pipeline::Blocked) -> Error {
    Error::not_implemented(format!(
        "an operator blocked {blocked} and the pull tree has nothing else to run, which is F4"
    ))
}
