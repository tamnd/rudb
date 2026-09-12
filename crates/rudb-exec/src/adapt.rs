//! Driving a push operator from the pull tree, until there is no pull tree left.
//!
//! The operators move behind the push traits one at a time rather than in one commit, which means
//! there is a period where a [`Stream`] has to sit in a tree whose other nodes still have
//! [`Operator::next`]. This is the one place that knows how to do that. It pulls a chunk from
//! below, pushes it through the stream, and hands back whatever came out.
//!
//! It is temporary and it is not a second interface. When the last operator has moved, the tree is
//! built as a pipeline, [`rudb_pipeline::run_serial`] runs it, and this file goes away with the
//! rest of the pull side. What survives is the [`Stream`] implementations, which is the point of
//! moving them first.

use std::fmt;

use rudb_common::{Error, Result};
use rudb_pipeline::{Progress, Stream};
use rudb_vector::Chunk;

use crate::operator::Operator;
use crate::schema::Schema;

/// One streaming operator with the tree below it.
pub(crate) struct Streamed<'a, S: Stream> {
    input: Box<dyn Operator + 'a>,
    stream: S,
    local: S::Local,
    schema: Schema,
    done: bool,
}

impl<'a, S: Stream> Streamed<'a, S> {
    /// `schema` is what this operator produces, which for a filter or a limit is the input's and
    /// for a projection is its own.
    pub(crate) fn new(input: Box<dyn Operator + 'a>, stream: S, schema: Schema) -> Self {
        let local = stream.local();
        Self { input, stream, local, schema, done: false }
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
            let Some(mut chunk) = self.input.next()? else { break };
            match self.stream.push(&mut chunk, &mut self.local)? {
                Progress::Done => self.done = true,
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

/// The error a blocked stream gets here.
///
/// Nothing in the tree returns [`Progress::Blocked`] yet, and the operators that will are the ones
/// that wait on io or on memory rather than the three streaming ones. It is an error rather than a
/// panic because a wrong answer is worse than a failed query, and it names the reason because that
/// is the useful half of the report.
fn parked(blocked: &rudb_pipeline::Blocked) -> Error {
    Error::not_implemented(format!(
        "a streaming operator blocked {blocked} and the pull tree has nothing else to run, \
         which is F4"
    ))
}
