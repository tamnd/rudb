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
//! the storage layer, the zone maps and the pushed down filters out of this crate. An aggregate
//! over a scan runs on as many workers as the scan has threads, each with its own state and its
//! own [`Rt`] made by [`Rt::worker`], and the workers' groups are merged into the query's when
//! they finish. A pipeline that produces rows runs the same way when only a sort reads them, and
//! its workers' rows are put together in the order the workers finish. Any other pipeline that
//! produces rows, or builds a join table, runs on one worker, because the order its rows come out
//! in is part of the answer.
//!
//! The breakers between pipelines are run here over the rows the pipeline before them produced,
//! and so is the fetch that reads whole rows back once a top N has picked them. A sort reads its
//! keys as numbers or bytes where their type orders that way, and as values compared by
//! `rudb_kernels::compare` where it does not.

#![allow(unsafe_code)]

mod feed;
mod finish;
mod merge;
mod tier;

use std::time::Instant;

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, LogicalType, Memory, Result, Session};
use rudb_pipeline::{Pool, Progress};
use rudb_plan::{Node, NodeRef, Plan};
use rudb_qc_gen::Query;
use rudb_qc_pipe::{Graph, Source, Stage};
use rudb_qc_plan::Kind;
pub use rudb_qc_plan::Refusal;
use rudb_qc_rt::Rt;
use rudb_vector::Chunk;

use crate::feed::Feed;
use crate::tier::Tiers;
pub use crate::tier::{Options, Report, Switch, Switches, Tier, moved_up};

/// A query the compiled engine has agreed to run.
#[derive(Debug)]
pub struct Compiled {
    graph: Graph,
    query: Query,
    tiers: Tiers,
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
    /// How many times the query moved between the tiers, which only `SET qc_switch` makes it do.
    pub switches: Switches,
    /// What the tiers did, with the compiles the run made.
    pub report: Report,
}

/// Compiles an optimized plan, or says why the compiled engine will not run it.
///
/// # Errors
///
/// A refusal from the physical plan, the generator or the driver itself. None of them is an error
/// in the query: the caller runs the first engine instead and logs the refusal.
pub fn compile(plan: &Plan, cancel: &Cancel) -> std::result::Result<Compiled, Refusal> {
    compile_with(plan, cancel, Options::default())
}

/// [`compile`], on the tier `options` asks for.
///
/// # Errors
///
/// As for [`compile`]. A function the tier does not lower is not a refusal: it runs on `interp`,
/// and [`Compiled::report`] says so.
pub fn compile_with(
    plan: &Plan,
    cancel: &Cancel,
    options: Options,
) -> std::result::Result<Compiled, Refusal> {
    let started = Instant::now();
    let rel = rudb_qc_plan::lower(plan)?;
    let graph = rudb_qc_pipe::split(&rel);
    check(&graph)?;
    let planned = started.elapsed();
    let mut rt = Rt::new(cancel.clone());
    let query = rudb_qc_gen::generate(&graph, &mut rt)?;
    let generated = started.elapsed().saturating_sub(planned);
    let tiers = Tiers::new(&query.module, options);
    tiers.generated_in(planned, generated);
    Ok(Compiled { graph, query, tiers, rt })
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
    /// The stages, one line each, the generated module and the [`Report`], for `EXPLAIN (CODEGEN)`.
    #[must_use]
    pub fn explain(&self) -> String {
        self.tiers.prepare_all(&self.query.module);
        format!(
            "{}\n{}\n{}",
            self.graph,
            rudb_qc_ir::print::print(&self.query.module),
            self.report()
        )
    }

    /// What the second tier did with the module so far. A pipeline's function is compiled when
    /// the pipeline starts, so before [`Compiled::run`] this has the planning and nothing else,
    /// and [`Answer::report`] has the rest.
    #[must_use]
    pub fn report(&self) -> Report {
        self.tiers.report()
    }

    /// Runs the query.
    ///
    /// # Errors
    ///
    /// Whatever the query raises: an overflow, a failed cast, a cancel, or an error from the scan
    /// underneath.
    pub fn run(mut self, plan: &Plan, under: Under<'_>) -> Result<Answer> {
        let mut outputs: Vec<Option<Vec<Chunk>>> = Vec::with_capacity(self.graph.stages.len());
        // The stages whose rows only a sort reads, which may come in any order.
        let mut unordered = vec![false; self.graph.stages.len()];
        // The stages whose rows only a top N reads, with its keys and how many rows it keeps.
        let mut topped = vec![None; self.graph.stages.len()];
        for stage in &self.graph.stages {
            if let Stage::Sort { input, .. } | Stage::TopN { input, .. } = stage {
                unordered[*input] = true;
            }
            if let Stage::TopN { input, keys, count, offset, .. } = stage {
                topped[*input] = Some((keys.as_slice(), count.saturating_add(*offset)));
            }
        }
        for (at, stage) in self.graph.stages.iter().enumerate() {
            let chunks = match stage {
                Stage::Pipeline(p) => {
                    let body = self.query.bodies[at]
                        .as_ref()
                        .ok_or_else(|| rudb_common::Error::internal("a pipeline with no body"))?;
                    // An earlier stage's rows are taken before the pipeline starts, because how
                    // many there are is what decides its tier.
                    let input = match &p.source {
                        Source::Stage { stage, .. } => Some(take(&mut outputs, *stage)?),
                        Source::Scan { .. } | Source::Values { .. } => None,
                    };
                    let rows = match &p.source {
                        Source::Scan { node, .. } => scanned(plan, *node, under.catalog),
                        Source::Values { rows, .. } => Some(rows.len()),
                        Source::Stage { .. } => {
                            input.as_ref().map(|chunks| chunks.iter().map(Chunk::len).sum())
                        }
                    };
                    if let Some(f) = self.tiers.func(&body.func) {
                        let probes = !body.probes.is_empty();
                        self.tiers.prepare(&self.query.module, f, rows, probes);
                    }
                    let feed = Feed::new(
                        &self.query.module,
                        &self.tiers,
                        p,
                        body,
                        &mut self.rt,
                        under.cancel,
                        unordered[at],
                    )?;
                    let feed = feed.sized(rows);
                    let feed = match topped[at] {
                        Some((keys, count)) => feed.topped(keys, count),
                        None => feed,
                    };
                    match &p.source {
                        Source::Scan { node, .. } => {
                            let mut scan = plan.clone();
                            scan.set_root(*node);
                            feed.scan(&scan, under)?;
                        }
                        Source::Values { rows, columns } => {
                            feed.push(&finish::values(rows, columns)?)?;
                        }
                        Source::Stage { .. } => {
                            for chunk in input.unwrap_or_default() {
                                if feed.push(&chunk)? == Progress::Done {
                                    break;
                                }
                            }
                        }
                    }
                    feed.finish()?
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
            switches: self.tiers.switches(),
            report: self.tiers.report(),
        })
    }
}

/// How many rows the table a scan reads has, when the scan reads a table and not a function.
fn scanned(plan: &Plan, node: NodeRef, catalog: &Catalog) -> Option<usize> {
    let Node::Get { catalog: c, schema, table, .. } = *plan.node(node) else {
        return None;
    };
    let name = QualifiedName::new(plan.string(c), plan.string(schema), plan.string(table));
    catalog.table(&name).ok().map(|t| t.rows().len())
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
