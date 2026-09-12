//! Turning a bound plan into a tree of operators.
//!
//! One match, one arm per logical operator, and nothing else. There is no physical plan and no cost
//! based choice between two ways of running the same node, which is the honest description of tier
//! 0: there is one implementation of each operator so there is nothing to choose between. The
//! physical planner that section 9.6 describes goes here, and the reason this is a separate module
//! from the operators is so that it can grow into one without any of them moving.
//!
//! The tree borrows the plan and the catalog for as long as it exists. A scan reads its rows out of
//! the catalog's table rather than copying them, and an expression reads its constants, its function
//! names and its types out of the plan's arena, so a plan that outlives the query it built is the
//! whole of the lifetime story here.
//!
//! # Where the measurement comes from
//!
//! Every operator this module makes is wrapped in [`Watched`] before it goes into the tree, and the
//! counters it reports into are registered with the [`Report`] the caller passed in. That is the
//! only place the wrapping happens, which is what makes it impossible for an operator to be left
//! out: an arm that forgets to wrap is an arm that does not compile, because the id it was handed
//! has to go somewhere.
//!
//! The ids are allocated as the match walks down, so operator 0 is the root and a child always has
//! a larger id than its parent. The pipeline numbers come from the same walk. The current pipeline
//! is the one the node's output flows into, and every arm that has a sink in it starts a new
//! pipeline below itself and records the edge: a sort is a pipeline that ends in the sort and a
//! pipeline that starts from the sorted rows, and the second cannot begin until the first is done.
//! There is no scheduler reading those edges yet. They are written down now because they are known
//! now, and a dependency reconstructed later from a tree somebody has already flattened is a
//! dependency somebody has to guess at.

use std::cell::Cell;
use std::sync::Arc;

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, Memory, Result};
use rudb_functions::TableFunction;
use rudb_metrics::{Counters, Report};
use rudb_pipeline::{Source, Watched};
use rudb_plan::{Node, NodeRef, Plan};

use crate::adapt::{Broken, Fed, Paired, Pulled, Streamed};
use crate::cancel::Guarded;
use crate::gather::{Gather, Keep};
use crate::group::{Aggregate, Distinct};
use crate::join::{CrossProduct, Gathered, Join};
use crate::operator::Operator;
use crate::schema::Schema;
use crate::setop::SetOp;
use crate::sort::Sort;
use crate::source::{Dummy, FileScan, Scan, Series, Values};
use crate::strategies::Strategies;
use crate::stream::{Filter, Limit, Project};
use crate::topn::TopN;

/// Builds the operator tree for a plan's root, for a query nothing will stop.
///
/// # Errors
///
/// If the plan names a table or a column the catalog does not have, if an expression is malformed
/// in a way [`Plan::validate`] would have caught, or anything an operator's construction reports.
pub fn build<'a>(plan: &'a Plan, catalog: &'a Catalog) -> Result<Box<dyn Operator + 'a>> {
    build_with(plan, catalog, &Cancel::new(), &Memory::unlimited())
}

/// Builds the operator tree for a plan's root, stoppable through this token and held to this
/// budget.
///
/// Every node in the tree is wrapped in a check, so the query stops at the first chunk boundary
/// after the token says to. See the `cancel` module for why the check is uniform rather than
/// placed in the operators that can loop.
///
/// The budget is not uniform, and that is the difference between the two. A streaming operator
/// holds one chunk and gives it away again, so charging every node would count the same megabyte
/// once per level of the tree. Only the operators that buffer without bound take a reservation, and
/// [`rudb_common::Memory`] lists which ones those are.
///
/// The measurement still happens. It goes into a report nobody reads, because the alternative is
/// two builders that drift apart, and a pair of clock readings per chunk is not a cost worth
/// avoiding by having a second one.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_with<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
) -> Result<Box<dyn Operator + 'a>> {
    build_measured(plan, catalog, cancel, memory, &Report::new())
}

/// Builds the operator tree, reporting what every operator in it did into `report`.
///
/// The report is what the caller keeps. Once the tree has been drained,
/// [`Report::fill`] turns it into the operator and pipeline rows of a metrics document, and that
/// document is the same one `EXPLAIN ANALYZE` prints and `--metrics` writes.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_measured<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    report: &Report,
) -> Result<Box<dyn Operator + 'a>> {
    let building = Building {
        plan,
        catalog,
        cancel,
        memory,
        report,
        next_operator: Cell::new(0),
        next_pipeline: Cell::new(1),
    };
    report.pipeline(ROOT);
    building.node(ROOT, plan.root())
}

/// The pipeline the root of the plan produces its rows into.
const ROOT: u32 = 0;

/// What the walk down the plan carries with it.
///
/// The two counters are cells rather than a mutable borrow because every arm of the match below
/// recurses while it is holding something it made, and threading a `&mut` through that would mean
/// building each node in two halves for no reason a reader of the arms would enjoy.
struct Building<'a, 'b> {
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &'b Cancel,
    memory: &'b Memory,
    report: &'b Report,
    next_operator: Cell<u32>,
    next_pipeline: Cell<u32>,
}

impl<'a> Building<'a, '_> {
    /// The id for the next operator, taken before its children are built so that a parent's id is
    /// smaller than every id below it.
    fn id(&self) -> u32 {
        let id = self.next_operator.get();
        self.next_operator.set(id + 1);
        id
    }

    /// The id for the next pipeline, which nothing depends on yet.
    fn fresh(&self) -> u32 {
        let id = self.next_pipeline.get();
        self.next_pipeline.set(id + 1);
        id
    }

    /// A new pipeline that `pipeline` cannot start until has finished.
    fn under(&self, pipeline: u32) -> u32 {
        let id = self.fresh();
        self.report.depends(pipeline, id);
        id
    }

    /// The counters for one operator, registered with the report.
    ///
    /// Everything built here is marked as a reference implementation, because at tier 0 everything
    /// built here is one. That is not a placeholder: the marker is what stops a number measured
    /// against the simplest correct version of an operator from being quoted as if it came from the
    /// fast one, and it comes off an operator on the day that operator gets a second tier.
    fn watch(&self, id: u32, pipeline: u32, kind: &str, detail: Option<&str>) -> Arc<Counters> {
        let counters = Counters::new(id, pipeline, kind).reference();
        let counters = match detail {
            Some(detail) => counters.detailed(detail),
            None => counters,
        };
        self.report.watch(counters)
    }

    fn node(&self, pipeline: u32, reference: NodeRef) -> Result<Box<dyn Operator + 'a>> {
        let plan = self.plan;
        let memory = self.memory;
        let inner: Box<dyn Operator + 'a> = match *plan.node(reference) {
            Node::Get { catalog: database, schema, table, index, columns, .. } => {
                let id = self.id();
                let name = QualifiedName::new(
                    plan.string(database),
                    plan.string(schema),
                    plan.string(table),
                );
                let scan = Scan::new(plan, self.catalog.table(&name)?, index, columns)?;
                let schema = scan.schema().clone();
                let counters = self.watch(id, pipeline, "Scan", Some(plan.string(table)));
                pulled(Watched::new(scan, counters), schema)
            }
            Node::Dummy => {
                let id = self.id();
                let dummy = Dummy::new();
                let schema = dummy.schema().clone();
                pulled(Watched::new(dummy, self.watch(id, pipeline, "Dummy", None)), schema)
            }
            Node::Values { index, columns, rows } => {
                let id = self.id();
                let values = Values::new(plan, index, columns, rows)?;
                let schema = values.schema().clone();
                pulled(Watched::new(values, self.watch(id, pipeline, "Values", None)), schema)
            }
            Node::TableFunction { index, function, args, options, settings, columns } => {
                let id = self.id();
                let name = plan.string(function);
                match TableFunction::lookup(name) {
                    Some(function @ (TableFunction::ReadParquet | TableFunction::ReadCsv)) => {
                        let scan =
                            FileScan::new(plan, index, function, args, options, settings, columns)?;
                        let schema = scan.schema().clone();
                        let counters = self.watch(id, pipeline, "FileScan", Some(name));
                        pulled(Watched::new(scan, counters), schema)
                    }
                    Some(TableFunction::RudbStrategies) => {
                        let table = Strategies::new(plan, index, columns)?;
                        let schema = table.schema().clone();
                        let counters = self.watch(id, pipeline, "Strategies", None);
                        pulled(Watched::new(table, counters), schema)
                    }
                    _ => {
                        let series = Series::new(plan, index, name, args)?;
                        let schema = series.schema().clone();
                        let counters = self.watch(id, pipeline, "Series", Some(name));
                        pulled(Watched::new(series, counters), schema)
                    }
                }
            }
            Node::Filter { input, predicate } => {
                let id = self.id();
                let input = self.node(pipeline, input)?;
                let schema = input.schema().clone();
                let filter = Filter::new(plan, predicate, &schema)?;
                let counters = self.watch(id, pipeline, "Filter", None);
                Box::new(Streamed::new(input, Watched::new(filter, counters), schema))
            }
            Node::Project { input, index, exprs, names } => {
                let id = self.id();
                let input = self.node(pipeline, input)?;
                let project = Project::new(plan, input.schema(), index, exprs, names)?;
                let schema = project.schema().clone();
                let counters = self.watch(id, pipeline, "Project", None);
                Box::new(Streamed::new(input, Watched::new(project, counters), schema))
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                let id = self.id();
                let below = self.under(pipeline);
                let input = self.node(below, input)?;
                let (aggregate, out) =
                    Aggregate::new(plan, input.schema(), index, groups, aggregates, memory)?;
                let schema = aggregate.schema().clone();
                let counters = self.watch(id, below, "Aggregate", None);
                Box::new(Broken::new(input, Watched::new(aggregate, counters), out, schema))
            }
            Node::Sort { input, keys } => {
                let id = self.id();
                let below = self.under(pipeline);
                let input = self.node(below, input)?;
                let schema = input.schema().clone();
                let (sort, out) = Sort::new(plan, &schema, keys, memory)?;
                let counters = self.watch(id, below, "Sort", None);
                Box::new(Broken::new(input, Watched::new(sort, counters), out, schema))
            }
            Node::Limit { input, count, offset } => {
                let id = self.id();
                let input = self.node(pipeline, input)?;
                let schema = input.schema().clone();
                let limit = Limit::new(count, offset);
                let counters = self.watch(id, pipeline, "Limit", None);
                Box::new(Streamed::new(input, Watched::new(limit, counters), schema))
            }
            Node::TopN { input, keys, count, offset } => {
                let id = self.id();
                let below = self.under(pipeline);
                let input = self.node(below, input)?;
                let schema = input.schema().clone();
                let (top, out) = TopN::new(plan, &schema, keys, count, offset, memory)?;
                let counters = self.watch(id, below, "TopN", None);
                Box::new(Broken::new(input, Watched::new(top, counters), out, schema))
            }
            Node::Distinct { input, on } => {
                let id = self.id();
                let below = self.under(pipeline);
                let input = self.node(below, input)?;
                let schema = input.schema().clone();
                let (distinct, out) = Distinct::new(plan, &schema, on, memory)?;
                let counters = self.watch(id, below, "Distinct", None);
                Box::new(Broken::new(input, Watched::new(distinct, counters), out, schema))
            }
            Node::Join { left, right, kind, conditions } => {
                let id = self.id();
                let gather_id = self.id();
                // The right side runs first, because no left row can be answered until every right
                // row it might match has been seen. That is the dependency edge, and it is the same
                // one the hash join builds on. The probing side is a pipeline of its own rather than
                // part of the one above it, because it ends in a sink, and it waits for the build
                // side.
                let build = self.fresh();
                let probe = self.fresh();
                self.report.depends(probe, build);
                self.report.depends(pipeline, probe);
                let right = self.node(build, right)?;
                let left = self.node(probe, left)?;
                let (gather, gathered) = Gather::new(memory);
                let side = Gathered { schema: right.schema(), rows: gathered };
                let (join, out) =
                    Join::new(plan, left.schema(), side, kind, conditions, self.cancel, memory);
                let schema = join.schema().clone();
                let kept = self.watch(gather_id, build, "Gather", None);
                let counters = self.watch(id, probe, "Join", None);
                Box::new(Paired::new(
                    right,
                    Watched::new(gather, kept),
                    left,
                    Watched::new(join, counters),
                    out,
                    schema,
                ))
            }
            Node::CrossProduct { left, right } => {
                let id = self.id();
                let keep_id = self.id();
                // The right side runs first and is kept as the chunks it arrived in, because it is
                // replayed once per left row. The left side streams, which is the whole point of
                // this operator: the product is produced a chunk at a time and never held, so the
                // product stays in the pipeline the left rows came from rather than starting one.
                let aside = self.under(pipeline);
                let right = self.node(aside, right)?;
                let left = self.node(pipeline, left)?;
                let (keep, kept) = Keep::new(memory);
                let cross = CrossProduct::new(left.schema(), right.schema(), kept);
                let schema = cross.schema().clone();
                let held = self.watch(keep_id, aside, "Keep", None);
                let counters = self.watch(id, pipeline, "CrossProduct", None);
                Box::new(Fed::new(
                    right,
                    Watched::new(keep, held),
                    Streamed::new(left, Watched::new(cross, counters), schema),
                ))
            }
            Node::SetOp { left, right, kind, all, index } => {
                let id = self.id();
                let gather_id = self.id();
                // The right side runs first, because nothing can be said about a left row until the
                // whole right side has been counted. That is the dependency edge, spelled out.
                let counting = self.fresh();
                let matching = self.fresh();
                self.report.depends(matching, counting);
                self.report.depends(pipeline, matching);
                let right = self.node(counting, right)?;
                let left = self.node(matching, left)?;
                let (gather, gathered) = Gather::new(memory);
                let (setop, out) = SetOp::new(left.schema(), gathered, kind, all, index, memory);
                let schema = setop.schema().clone();
                let kept = self.watch(gather_id, counting, "Gather", None);
                let counters = self.watch(id, matching, "SetOp", None);
                Box::new(Paired::new(
                    right,
                    Watched::new(gather, kept),
                    left,
                    Watched::new(setop, counters),
                    out,
                    schema,
                ))
            }
        };
        Ok(Box::new(Guarded::new(inner, self.cancel.clone())))
    }
}

/// A leaf source with the adapter that pulls chunks out of it.
///
/// Every leaf is a [`Source`] and everything above it still pulls, so this is
/// where the two meet. The schema is passed in rather than asked for through a trait, because a
/// source says what it produces on its own type and adding a trait method to say it again would be
/// a second answer to the same question.
fn pulled<'a, S: Source + 'a>(source: S, schema: Schema) -> Box<dyn Operator + 'a> {
    Box::new(Pulled::new(source, schema))
}
