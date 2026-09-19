//! A built query: the pipelines it runs, in the order they have to run, and the queue the rows come
//! out of.
//!
//! This is what replaced the pull tree. A plan used to become a tree of operators whose root was
//! pulled from, and each pipeline breaker in it drained the tree below it on the first pull. The
//! order that produced was right, because a breaker cannot answer until its input is finished, but
//! it was an order the call stack happened to have rather than one anybody wrote down. Here it is
//! written down: [`Query::run`] takes the pipelines in dependency order and runs each of them to
//! completion.
//!
//! # Where the threads are
//!
//! Inside one pipeline and not across them. Each pipeline runs on as many threads as
//! [`Pipeline::degree`] says, which is bounded by what the database's [`Pool`] will lend, by
//! whether every operator in it will run as more than one instance, and by how many morsels its
//! source has. Then the next one starts.
//!
//! Running two pipelines of one query at the same time is the other kind of parallelism and it is
//! not here. The dependency edges say which pairs could overlap, so the information is already
//! written down, and what is missing is a scheduler that holds several pipelines at once rather
//! than a driver that is handed one. It is also worth much less: the shapes in ClickBench are a
//! scan feeding an aggregate feeding a sort, which is a chain, and a chain has nothing to overlap.

use std::sync::Arc;

use rudb_common::{Cancel, Error, Result};
use rudb_metrics::Driver;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rudb_pipeline::{Pipeline, Pool, RootReader, run_parallel};
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
    reader: Option<RootReader>,
    /// What the query produces.
    schema: Schema,
    /// CPU nanoseconds burned on threads other than the one that called [`Query::run`].
    worker_cpu_ns: AtomicU64,
    /// The most instances any one pipeline ran as.
    widest: AtomicUsize,
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
        reader: Option<RootReader>,
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
        Ok(Self {
            pipelines,
            drivers,
            reader,
            schema,
            worker_cpu_ns: AtomicU64::new(0),
            widest: AtomicUsize::new(0),
        })
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
    /// The lease is taken per pipeline and given back at the end of it, so a query whose scan uses
    /// nine threads and whose sort uses one holds nine for as long as the scan and one after that,
    /// and the threads it is not using are there for whatever else the database is running.
    ///
    /// How many threads it borrows and how many instances it runs are two numbers. The instances
    /// are what the source has work for. The borrow is the wider of that and what the sink says it
    /// can finish on, because the finish happens on the same threads with every instance already
    /// joined, and a hash aggregate merging a million groups is not the same width as the scan that
    /// fed it.
    ///
    /// # Errors
    ///
    /// Whatever any operator reports, or [`ErrorCode::Interrupt`](rudb_common::ErrorCode::Interrupt)
    /// if the token says to stop. The check is per chunk, in the driver, which is why no operator
    /// here holds a token of its own except the join, whose nested loop can outlive a chunk.
    pub fn run(&self, cancel: &Cancel, pool: &Pool) -> Result<()> {
        for (pipeline, driver) in self.pipelines.iter().zip(&self.drivers) {
            let lease = pool.lease(pipeline.lease_degree(pool.threads()));
            let degree = pipeline.degree(pool.threads()).min(lease.degree());
            let spread = {
                let _running = driver.running();
                run_parallel(pipeline, cancel, &lease, degree)?
            };
            driver.ran(degree, spread.worker_cpu_ns);
            driver.waited(spread.slowest_ns, spread.slowest_cpu_ns, spread.finalize_ns);
            self.worker_cpu_ns.fetch_add(spread.worker_cpu_ns, Ordering::Relaxed);
            self.widest.fetch_max(degree, Ordering::Relaxed);
        }
        Ok(())
    }

    /// CPU nanoseconds this query burned on threads other than the one that ran it.
    ///
    /// A caller timing the execution reads its own thread's CPU clock, which is the only clock
    /// there is that attributes work to the thread that did it, and which therefore cannot see the
    /// workers. This is what it missed.
    #[must_use]
    pub fn worker_cpu_ns(&self) -> u64 {
        self.worker_cpu_ns.load(Ordering::Relaxed)
    }

    /// The most instances any one pipeline of this query ran as.
    ///
    /// Not the setting and not an average. A query whose scan ran on nine threads and whose sort ran
    /// on one reports nine, because the question this answers is what the query was able to use.
    #[must_use]
    pub fn widest(&self) -> usize {
        self.widest.load(Ordering::Relaxed)
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
        let chunk = self
            .reader
            .as_ref()
            .ok_or_else(|| Error::internal("a query built into a sink has no result reader"))?
            .next_chunk()?;
        if let Some(chunk) = &chunk {
            chunk.validate_external()?;
        }
        Ok(chunk)
    }

    /// Runs the query and collects everything it produced.
    ///
    /// The convenience the tests and the simple callers want. A caller that cares about holding one
    /// chunk at a time calls [`Query::run`] and [`Query::next_chunk`] itself.
    ///
    /// # Errors
    ///
    /// The same as [`Query::run`].
    pub fn collect(&self, cancel: &Cancel, pool: &Pool) -> Result<Vec<Chunk>> {
        self.run(cancel, pool)?;
        let mut chunks = Vec::new();
        while let Some(chunk) = self.next_chunk()? {
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}
