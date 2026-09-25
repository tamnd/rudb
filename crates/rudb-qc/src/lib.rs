//! The compiled engine's driver, per `spec/compiler/05-pipelines-and-state.md` and
//! `spec/compiler/19-crate-layout.md`.
//!
//! [`compile`] takes an optimized plan down to a [`Compiled`] query: the physical plan, the
//! stages, the generated module and the runtime its handles live in. It refuses anything some
//! layer below does not know yet, and the caller runs the first engine instead. [`Compiled::run`]
//! then runs the stages in order and returns the answer as chunks.
//!
//! A pipeline over a base table reads it through the first engine. The plan is cloned with the
//! `Get` node as its root, built by `rudb_exec` into a query whose root is a sink of ours, and
//! every chunk the scan produces is handed to the compiled body as one morsel. That is what keeps
//! the storage layer, the zone maps and the pushed down filters out of this crate, and it is also
//! where C1 stops: the sink says it is not parallel, because one [`Rt`] serves the whole query and
//! per worker state is C3.
//!
//! The breakers between pipelines are run here over the rows the pipeline before them produced,
//! and so is the fetch that reads whole rows back once a top N has picked them.
//! A sort, a top N and a limit are cheap on ClickBench, where they sit over a few thousand groups
//! at most, and doing them a value at a time keeps the rules for comparing values in one place,
//! `rudb_kernels::compare`.

#![allow(unsafe_code)]

mod feed;
mod finish;

use rudb_catalog::Catalog;
use rudb_common::{Cancel, LogicalType, Memory, Result, Session};
use rudb_pipeline::Pool;
use rudb_plan::Plan;
use rudb_qc_gen::{Out, Query};
use rudb_qc_interp::Program;
use rudb_qc_pipe::{Graph, Source, Stage};
use rudb_qc_plan::Kind;
pub use rudb_qc_plan::Refusal;
use rudb_qc_rt::Rt;
use rudb_vector::Chunk;

use crate::feed::Feed;

/// A query the compiled engine has agreed to run.
#[derive(Debug)]
pub struct Compiled {
    graph: Graph,
    query: Query,
    program: Program,
    rt: Rt,
}

/// What a query produced: the column names and types and the rows.
#[derive(Debug)]
pub struct Answer {
    /// The column names.
    pub names: Vec<String>,
    /// The column types.
    pub types: Vec<LogicalType>,
    /// The rows.
    pub chunks: Vec<Chunk>,
}

/// Compiles an optimized plan, or says why the compiled engine will not run it.
///
/// # Errors
///
/// A refusal from the physical plan, the generator or the driver itself. None of them is an error
/// in the query: the caller runs the first engine instead and logs the refusal.
pub fn compile(plan: &Plan, cancel: &Cancel) -> std::result::Result<Compiled, Refusal> {
    let rel = rudb_qc_plan::lower(plan)?;
    let graph = rudb_qc_pipe::split(&rel);
    check(&graph)?;
    let mut rt = Rt::new(cancel.clone());
    let query = rudb_qc_gen::generate(&graph, &mut rt)?;
    let program = Program::new(&query.module);
    Ok(Compiled { graph, query, program, rt })
}

/// Refuses what the driver cannot run yet.
fn check(graph: &Graph) -> std::result::Result<(), Refusal> {
    for stage in &graph.stages {
        match stage {
            Stage::Pipeline(_) | Stage::Limit { .. } => {}
            Stage::Sort { keys, .. } | Stage::TopN { keys, .. } => {
                if keys.iter().any(|k| !matches!(k.expr.kind, Kind::Column(_))) {
                    return Err(Refusal::new("Sort", "a sort key that is not a column"));
                }
            }
            Stage::Fetch { row, .. } => {
                if !matches!(row.kind, Kind::Column(_)) {
                    return Err(Refusal::new("TableFetch", "a row ordinal that is not a column"));
                }
            }
        }
    }
    Ok(())
}

/// What a run needs from the database: the catalog the scans read, and the budget and settings the
/// first engine builds them under.
#[derive(Clone, Copy, Debug)]
pub struct Under<'a> {
    /// The catalog.
    pub catalog: &'a Catalog,
    /// Stops the query.
    pub cancel: &'a Cancel,
    /// The memory budget.
    pub memory: &'a Memory,
    /// The seam settings.
    pub seams: &'a rudb_seam::Settings,
    /// The session.
    pub session: &'a Session,
    /// The threads.
    pub pool: &'a Pool,
}

impl Compiled {
    /// The stages, one line each, and the generated module, for `EXPLAIN (CODEGEN)`.
    #[must_use]
    pub fn explain(&self) -> String {
        format!("{}\n{}", self.graph, rudb_qc_ir::print::print(&self.query.module))
    }

    /// Runs the query.
    ///
    /// # Errors
    ///
    /// Whatever the query raises: an overflow, a failed cast, a cancel, or an error from the scan
    /// underneath.
    pub fn run(mut self, plan: &Plan, under: Under<'_>) -> Result<Answer> {
        let mut outputs: Vec<Option<Vec<Chunk>>> = Vec::with_capacity(self.graph.stages.len());
        for (at, stage) in self.graph.stages.iter().enumerate() {
            let chunks = match stage {
                Stage::Pipeline(p) => {
                    let body = self.query.bodies[at]
                        .as_ref()
                        .ok_or_else(|| rudb_common::Error::internal("a pipeline with no body"))?;
                    let feed = Feed::new(
                        &self.query.module,
                        &self.program,
                        body,
                        &mut self.rt,
                        under.cancel,
                    )?;
                    match &p.source {
                        Source::Scan { node, .. } => {
                            let mut scan = plan.clone();
                            scan.set_root(*node);
                            feed.scan(&scan, under)?;
                        }
                        Source::Values { rows, columns } => {
                            feed.push(&finish::values(rows, columns)?)?;
                        }
                        Source::Stage { stage, .. } => {
                            for chunk in take(&mut outputs, *stage)? {
                                feed.push(&chunk)?;
                            }
                        }
                    }
                    let out = feed.finish()?;
                    match &body.sink {
                        Out::Result { .. } => out,
                        Out::Aggregate(g) => finish::groups(&self.rt, g, stage.columns())?,
                    }
                }
                Stage::Sort { input, keys, .. } => {
                    finish::sort(take(&mut outputs, *input)?, keys, None, 0)?
                }
                Stage::TopN { input, keys, count, offset, .. } => {
                    finish::sort(take(&mut outputs, *input)?, keys, Some(*count), *offset)?
                }
                Stage::Limit { input, count, offset, .. } => {
                    finish::limit(take(&mut outputs, *input)?, *count, *offset)?
                }
                Stage::Fetch { input, node, row, .. } => {
                    let Kind::Column(ordinal) = row.kind else {
                        return Err(rudb_common::Error::internal("a fetch the check let through"));
                    };
                    finish::fetch(take(&mut outputs, *input)?, plan, *node, ordinal, under.catalog)?
                }
            };
            outputs.push(Some(chunks));
        }
        let columns = self.graph.columns();
        let chunks = outputs.pop().unwrap_or_default().unwrap_or_default();
        Ok(Answer {
            names: columns.iter().map(|c| c.name.clone()).collect(),
            types: columns.iter().map(|c| c.ty.clone()).collect(),
            chunks,
        })
    }
}

/// The rows of an earlier stage, which only the stage after it reads.
fn take(outputs: &mut [Option<Vec<Chunk>>], stage: usize) -> Result<Vec<Chunk>> {
    outputs
        .get_mut(stage)
        .and_then(Option::take)
        .ok_or_else(|| rudb_common::Error::internal(format!("stage {stage} was read twice")))
}

#[cfg(test)]
mod tests;
