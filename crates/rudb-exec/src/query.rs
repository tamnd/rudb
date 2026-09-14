//! A built query: the pipelines it runs, in the order they have to run, and the queue the rows come
//! out of.
//!
//! This is what replaced the pull tree. A plan used to become a tree of operators whose root was
//! pulled from, and each pipeline breaker in it drained the tree below it on the first pull. The
//! order that produced was right, because a breaker cannot answer until its input is finished, but
//! it was an order the call stack happened to have rather than one anybody wrote down. Here it is
//! written down: [`Query::run`] takes the pipelines in dependency order and runs each of them to
//! completion, and the only reason it is still one after another is that the driver it calls is the
//! single threaded one.
//!
//! # What the next milestone changes
//!
//! One line. [`run_serial`] becomes a scheduler that runs several instances of one pipeline on
//! several threads, and it is handed the same [`Pipeline`] values this holds. Nothing about the way
//! a query is built has to move for that, which is the whole reason for cutting the tree up now
//! rather than at the same time.

use std::sync::Arc;

use rudb_common::{Cancel, Error, Result};
use rudb_metrics::Driver;
use rudb_pipeline::{Pipeline, RootReader, run_serial};
use rudb_vector::Chunk;

use crate::schema::Schema;

/// A plan that has been built and is ready to run.
///
/// It borrows the plan and the catalog it was built from, which is what `'a` is. A scan reads its
/// rows out of the catalog's table rather than copying them and an expression reads its constants
/// out of the plan's arena, so a query cannot outlive either.
#[derive(Debug)]
pub struct Query<'a> {
    /// The pipelines, in an order where everything a pipeline waits for comes before it.
    pipelines: Vec<Pipeline<'a>>,
    /// The driver counters for each pipeline, in the same order.
    drivers: Vec<Arc<Driver>>,
    /// Where the last pipeline puts its rows.
    reader: RootReader,
    /// What the query produces.
    schema: Schema,
}

impl<'a> Query<'a> {
    /// A query over pipelines that are already in dependency order, each paired with its driver.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a pipeline waits for one that
    /// does not come before it. That is a builder bug rather than anything a query can cause, and it
    /// is checked here because running the pipelines in the wrong order reads a buffer nobody has
    /// filled yet and answers with no rows rather than failing.
    pub(crate) fn new(
        pipelines: Vec<Pipeline<'a>>,
        drivers: Vec<Arc<Driver>>,
        reader: RootReader,
        schema: Schema,
    ) -> Result<Self> {
        for (at, pipeline) in pipelines.iter().enumerate() {
            for waited in pipeline.depends_on() {
                let before = pipelines[..at].iter().any(|earlier| earlier.id() == *waited);
                if !before {
                    return Err(Error::internal(format!(
                        "{} waits for {waited}, which the builder did not put before it",
                        pipeline.id()
                    )));
                }
            }
        }
        Ok(Self { pipelines, drivers, reader, schema })
    }

    /// The columns this query produces.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// How many pipelines the query runs.
    #[must_use]
    pub fn pipelines(&self) -> usize {
        self.pipelines.len()
    }

    /// Runs every pipeline, stopping at the first one that fails.
    ///
    /// Each one is timed against its own driver, which is the loop that runs a pipeline rather than
    /// any operator in it. That time is not nothing: on a scan of ten million rows the loop goes
    /// round ten thousand times, and none of it sits inside an operator's own span, so without a
    /// driver it is time the metrics document cannot account for.
    ///
    /// # Errors
    ///
    /// Whatever any operator reports, or [`ErrorCode::Interrupt`](rudb_common::ErrorCode::Interrupt)
    /// if the token says to stop. The check is per chunk, in the driver, which is why no operator
    /// here holds a token of its own except the join, whose nested loop can outlive a chunk.
    pub fn run(&self, cancel: &Cancel) -> Result<()> {
        for (pipeline, driver) in self.pipelines.iter().zip(&self.drivers) {
            let _running = driver.running();
            run_serial(pipeline, cancel)?;
        }
        Ok(())
    }

    /// The next chunk of the answer, or `None` when there are no more.
    ///
    /// Only meaningful after [`Query::run`] has returned. The serial driver runs a pipeline to
    /// completion, so everything the query produced is queued by then, and taking a chunk here
    /// removes it from the queue rather than copying it out.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Internal`](rudb_common::ErrorCode::Internal) if a thread panicked while holding
    /// the queue.
    pub fn next_chunk(&self) -> Result<Option<Chunk>> {
        self.reader.next_chunk()
    }

    /// Runs the query and collects everything it produced.
    ///
    /// The convenience the tests and the simple callers want. A caller that cares about holding one
    /// chunk at a time calls [`Query::run`] and [`Query::next_chunk`] itself.
    ///
    /// # Errors
    ///
    /// The same as [`Query::run`].
    pub fn collect(&self, cancel: &Cancel) -> Result<Vec<Chunk>> {
        self.run(cancel)?;
        let mut chunks = Vec::new();
        while let Some(chunk) = self.next_chunk()? {
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}
