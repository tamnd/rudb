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
//! Neither the ids nor the pipeline numbers are worked out here. They come from [`Shape`], which is
//! one walk over the plan in `rudb-plan`, because `EXPLAIN` prints the same numbering and the same
//! decomposition without building anything, and two versions of that rule would be right on the day
//! they were written and disagree some time after. What this module does is ask which operator a
//! node is and wrap it.

use std::sync::Arc;

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, Memory, Result};
use rudb_functions::TableFunction;
use rudb_metrics::{Counters, Report};
use rudb_pipeline::{Source, Watched};
use rudb_plan::{Node, NodeRef, Plan, Shape};

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
    let shape = Shape::of(plan);
    for pipeline in shape.all() {
        report.pipeline(pipeline);
        for waits_for in shape.waits_for(pipeline) {
            report.depends(pipeline, *waits_for);
        }
    }
    let building = Building { plan, catalog, cancel, memory, report, shape };
    building.node(plan.root())
}

/// What the walk down the plan carries with it.
struct Building<'a, 'b> {
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &'b Cancel,
    memory: &'b Memory,
    report: &'b Report,
    shape: Shape,
}

impl<'a> Building<'a, '_> {
    /// The counters for one operator, registered with the report.
    ///
    /// Everything built here is marked as a reference implementation, because at tier 0 everything
    /// built here is one. That is not a placeholder: the marker is what stops a number measured
    /// against the simplest correct version of an operator from being quoted as if it came from the
    /// fast one, and it comes off an operator on the day that operator gets a second tier.
    /// The id of the operator holding the side of this node that has to finish first.
    ///
    /// # Panics
    ///
    /// If the node has one input, which is a node whose arm below should not have called this.
    fn gathered(&self, node: NodeRef) -> u32 {
        self.shape.gathered(node).expect("a node with two inputs has a second operator")
    }

    fn watch(&self, id: u32, pipeline: u32, kind: &str, detail: Option<&str>) -> Arc<Counters> {
        let counters = Counters::new(id, pipeline, kind).reference();
        let counters = match detail {
            Some(detail) => counters.detailed(detail),
            None => counters,
        };
        self.report.watch(counters)
    }

    fn node(&self, reference: NodeRef) -> Result<Box<dyn Operator + 'a>> {
        let plan = self.plan;
        let memory = self.memory;
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        let inner: Box<dyn Operator + 'a> = match *plan.node(reference) {
            Node::Get { catalog: database, schema, table, index, columns, .. } => {
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
                let dummy = Dummy::new();
                let schema = dummy.schema().clone();
                pulled(Watched::new(dummy, self.watch(id, pipeline, "Dummy", None)), schema)
            }
            Node::Values { index, columns, rows } => {
                let values = Values::new(plan, index, columns, rows)?;
                let schema = values.schema().clone();
                pulled(Watched::new(values, self.watch(id, pipeline, "Values", None)), schema)
            }
            Node::TableFunction { index, function, args, options, settings, columns } => {
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
                let input = self.node(input)?;
                let schema = input.schema().clone();
                let filter = Filter::new(plan, predicate, &schema)?;
                let counters = self.watch(id, pipeline, "Filter", None);
                Box::new(Streamed::new(input, Watched::new(filter, counters), schema))
            }
            Node::Project { input, index, exprs, names } => {
                let input = self.node(input)?;
                let project = Project::new(plan, input.schema(), index, exprs, names)?;
                let schema = project.schema().clone();
                let counters = self.watch(id, pipeline, "Project", None);
                Box::new(Streamed::new(input, Watched::new(project, counters), schema))
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                let input = self.node(input)?;
                let (aggregate, out) =
                    Aggregate::new(plan, input.schema(), index, groups, aggregates, memory)?;
                let schema = aggregate.schema().clone();
                let counters = self.watch(id, pipeline, "Aggregate", None);
                let made = Arc::clone(&counters);
                let driver = self.report.driving(pipeline);
                Box::new(Broken::new(
                    input,
                    Watched::new(aggregate, counters),
                    driver,
                    out,
                    made,
                    schema,
                ))
            }
            Node::Sort { input, keys } => {
                let input = self.node(input)?;
                let schema = input.schema().clone();
                let (sort, out) = Sort::new(plan, &schema, keys, memory)?;
                let counters = self.watch(id, pipeline, "Sort", None);
                let made = Arc::clone(&counters);
                let driver = self.report.driving(pipeline);
                Box::new(Broken::new(
                    input,
                    Watched::new(sort, counters),
                    driver,
                    out,
                    made,
                    schema,
                ))
            }
            Node::Limit { input, count, offset } => {
                let input = self.node(input)?;
                let schema = input.schema().clone();
                let limit = Limit::new(count, offset);
                let counters = self.watch(id, pipeline, "Limit", None);
                Box::new(Streamed::new(input, Watched::new(limit, counters), schema))
            }
            Node::TopN { input, keys, count, offset } => {
                let input = self.node(input)?;
                let schema = input.schema().clone();
                let (top, out) = TopN::new(plan, &schema, keys, count, offset, memory)?;
                let counters = self.watch(id, pipeline, "TopN", None);
                let made = Arc::clone(&counters);
                let driver = self.report.driving(pipeline);
                Box::new(Broken::new(input, Watched::new(top, counters), driver, out, made, schema))
            }
            Node::Distinct { input, on } => {
                let input = self.node(input)?;
                let schema = input.schema().clone();
                let (distinct, out) = Distinct::new(plan, &schema, on, memory)?;
                let counters = self.watch(id, pipeline, "Distinct", None);
                let made = Arc::clone(&counters);
                let driver = self.report.driving(pipeline);
                Box::new(Broken::new(
                    input,
                    Watched::new(distinct, counters),
                    driver,
                    out,
                    made,
                    schema,
                ))
            }
            Node::Join { left, right, kind, conditions } => {
                // The right side runs first, because no left row can be answered until every right
                // row it might match has been seen. That is the dependency edge, and it is the same
                // one the hash join builds on. The probing side is a pipeline of its own rather than
                // part of the one above it, because it ends in a sink, and it waits for the build
                // side.
                let gather_id = self.gathered(reference);
                let gathering = self.shape.pipeline(right);
                let right = self.node(right)?;
                let left = self.node(left)?;
                let (gather, gathered) = Gather::new(memory);
                let side = Gathered { schema: right.schema(), rows: gathered };
                let (join, out) =
                    Join::new(plan, left.schema(), side, kind, conditions, self.cancel, memory);
                let schema = join.schema().clone();
                let kept = self.watch(gather_id, gathering, "Gather", None);
                let counters = self.watch(id, pipeline, "Join", None);
                let made = Arc::clone(&counters);
                Box::new(Paired::new(
                    right,
                    Watched::new(gather, kept),
                    self.report.driving(gathering),
                    left,
                    Watched::new(join, counters),
                    self.report.driving(pipeline),
                    out,
                    made,
                    schema,
                ))
            }
            Node::CrossProduct { left, right } => {
                // The right side runs first and is kept as the chunks it arrived in, because it is
                // replayed once per left row. The left side streams, which is the whole point of
                // this operator: the product is produced a chunk at a time and never held, so the
                // product stays in the pipeline the left rows came from rather than starting one.
                let keep_id = self.gathered(reference);
                let aside = self.shape.pipeline(right);
                let right = self.node(right)?;
                let left = self.node(left)?;
                let (keep, kept) = Keep::new(memory);
                let cross = CrossProduct::new(left.schema(), right.schema(), kept);
                let schema = cross.schema().clone();
                let held = self.watch(keep_id, aside, "Keep", None);
                let counters = self.watch(id, pipeline, "CrossProduct", None);
                Box::new(Fed::new(
                    right,
                    Watched::new(keep, held),
                    self.report.driving(aside),
                    Streamed::new(left, Watched::new(cross, counters), schema),
                ))
            }
            Node::SetOp { left, right, kind, all, index } => {
                // The right side runs first, because nothing can be said about a left row until the
                // whole right side has been counted. That is the dependency edge, spelled out.
                let gather_id = self.gathered(reference);
                let counting = self.shape.pipeline(right);
                let right = self.node(right)?;
                let left = self.node(left)?;
                let (gather, gathered) = Gather::new(memory);
                let (setop, out) = SetOp::new(left.schema(), gathered, kind, all, index, memory);
                let schema = setop.schema().clone();
                let kept = self.watch(gather_id, counting, "Gather", None);
                let counters = self.watch(id, pipeline, "SetOp", None);
                let made = Arc::clone(&counters);
                Box::new(Paired::new(
                    right,
                    Watched::new(gather, kept),
                    self.report.driving(counting),
                    left,
                    Watched::new(setop, counters),
                    self.report.driving(pipeline),
                    out,
                    made,
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
