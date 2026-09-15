//! Turning a bound plan into the pipelines that run it.
//!
//! One match, one arm per logical operator, and nothing else. There is no physical plan and no cost
//! based choice between two ways of running the same node, which is the honest description of tier
//! 0: there is one implementation of each operator so there is nothing to choose between. The
//! physical planner that section 9.6 describes goes here, and the reason this is a separate module
//! from the operators is so that it can grow into one without any of them moving.
//!
//! The pipelines borrow the plan and the catalog for as long as they exist. A scan reads its rows
//! out of the catalog's table rather than copying them, and an expression reads its constants, its
//! function names and its types out of the plan's arena, so a plan that outlives the query it built
//! is the whole of the lifetime story here.
//!
//! # How a tree becomes a list
//!
//! The walk is the same one it always was, down from the root, and what changed is what it carries
//! back up. A node returns a [`Segment`], which is a source with the streaming operators stacked on
//! it so far, and a node that is a pipeline breaker closes the segment under it into a finished
//! [`Pipeline`] and starts a new segment over the buffer that breaker finalises into. So a plan with
//! two breakers in it comes back as three pipelines, and they are pushed onto the list in the order
//! they have to run, because a breaker's own pipeline is closed before the walk returns to whatever
//! is above it.
//!
//! A node with two inputs closes the side that has to finish first and then walks the side that uses
//! it, which is the same order the ids are handed out in and the same order the work happens in.
//!
//! # Where the measurement comes from
//!
//! Every operator this module makes is wrapped in [`Watched`] before it goes into a pipeline, and
//! the counters it reports into are registered with the [`Report`] the caller passed in. That is the
//! only place the wrapping happens, which is what makes it impossible for an operator to be left
//! out: an arm that forgets to wrap is an arm that does not compile, because the id it was handed
//! has to go somewhere.
//!
//! A breaker's counters go around two objects rather than one. The sink is the operator, and the
//! buffer the next pipeline sources from is where its rows come back out, so both are wrapped in the
//! same counters and a sort's row count is the rows it produced rather than zero.
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
use rudb_metrics::{Counters, Driver, Report};
use rudb_parquet::{Bound, Op};
use rudb_pipeline::{
    BufferId, DynSink, DynStream, Pipeline, PipelineId, Source, Watched, root, root_in_order,
};
use rudb_plan::{
    CompareOp, ConjunctionOp, Expr, ExprRef, Node, NodeRef, PipelineRef, Plan, ROOT, Shape, Slice,
    seams_of,
};
use rudb_seam::Settings;

use crate::fetch::Fetch;
use crate::gather::{Gather, Keep};
use crate::group::{Aggregate, Distinct};
use crate::join::{CrossProduct, Gathered, Join};
use crate::keywords::keywords;
use crate::query::Query;
use crate::register::registries;
use crate::schema::Schema;
use crate::setop::SetOp;
use crate::sort::Sort;
use crate::source::{Dummy, FileScan, Scan, Series, Values};
use crate::strategies::strategies;
use crate::stream::{Filter, Limit, Project};
use crate::topn::TopN;

/// Builds the pipelines for a plan's root, for a query nothing will stop.
///
/// Every seam is left at its default, which is what a caller with no session behind it wants and is
/// what the tests in this crate are written against.
///
/// # Errors
///
/// If the plan names a table or a column the catalog does not have, if an expression is malformed
/// in a way [`Plan::validate`] would have caught, or anything an operator's construction reports.
pub fn build<'a>(plan: &'a Plan, catalog: &'a Catalog) -> Result<Query<'a>> {
    build_with(plan, catalog, &Cancel::new(), &Memory::unlimited(), &Settings::new())
}

/// Builds the pipelines for a plan's root, stoppable through this token and held to this budget.
///
/// The token is checked once per chunk by the driver, so the query stops at the first chunk boundary
/// after the token says to. It is one check in one place rather than a decision per operator,
/// because a decision per operator is a decision somebody gets wrong when they add the twentieth
/// one. What the driver cannot see is work an operator does inside one call, and the join is the one
/// that can: its nested loop runs to the end inside a single push, and a hundred thousand left rows
/// against thirty thousand right ones is a minute with nothing looking at the token, so that loop
/// holds the token as well and checks it once per left row.
///
/// The budget is not uniform, and that is the difference between the two. A streaming operator holds
/// one chunk and gives it away again, so charging every operator would count the same megabyte once
/// per level. Only the operators that buffer without bound take a reservation, and
/// [`rudb_common::Memory`] lists which ones those are.
///
/// The measurement still happens. It goes into a report nobody reads, because the alternative is two
/// builders that drift apart, and a pair of clock readings per chunk is not a cost worth avoiding by
/// having a second one.
///
/// The seam settings are the session's with the statement's hints on top, and they are read here
/// rather than looked up later, because a choice made while the query is built is a choice `EXPLAIN`
/// can print before the query runs. An operator that sits on a seam chooses once, in its
/// constructor, and holds what it chose.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_with<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    seams: &Settings,
) -> Result<Query<'a>> {
    build_measured(plan, catalog, cancel, memory, seams, &Report::new())
}

/// Builds the pipelines, reporting what every operator in them did into `report`.
///
/// The report is what the caller keeps. Once the query has been run, [`Report::fill`] turns it into
/// the operator and pipeline rows of a metrics document, and that document is the same one
/// `EXPLAIN ANALYZE` prints and `--metrics` writes.
///
/// # Errors
///
/// The same as [`build`].
pub fn build_measured<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    seams: &Settings,
    report: &Report,
) -> Result<Query<'a>> {
    let shape = Shape::of(plan);
    for pipeline in shape.all() {
        report.pipeline(pipeline);
        for waits_for in shape.waits_for(pipeline) {
            report.depends(pipeline, *waits_for);
        }
    }
    let mut building = Building {
        plan,
        catalog,
        cancel,
        memory,
        seams,
        report,
        shape,
        done: Vec::new(),
        drivers: Vec::new(),
        pruning: Vec::new(),
    };
    let segment = building.node(plan.root())?;
    let schema = segment.schema.clone();
    // A query whose rows come out of a sort or a top n is already in the order somebody asked for,
    // and holding chunks back to restore the source order would only add latency to an order nobody
    // is going to look at. Everything else gets the root that puts them back, because the moment
    // several threads read the same file a plain `SELECT` would otherwise come back in a different
    // order on every run. It costs nothing to decide here and it means the scheduler never has to.
    let (sink, reader) = if ordered(plan, plan.root()) {
        root(BufferId(0), None)
    } else {
        root_in_order(BufferId(0), None)
    };
    building.close(segment, ROOT, Arc::new(sink));
    let Building { done, drivers, .. } = building;
    Query::new(done, drivers, reader, schema)
}

/// Whether the rows reaching the root are already in an order the plan chose.
///
/// A sort and a top n both decide one. Everything between them and the root either keeps the order
/// it was given or is not a node that can sit there, and the walk stops at the first node that is
/// neither.
fn ordered(plan: &Plan, node: NodeRef) -> bool {
    match *plan.node(node) {
        Node::Sort { .. } | Node::TopN { .. } => true,
        Node::Project { input, .. }
        | Node::Filter { input, .. }
        | Node::Limit { input, .. }
        | Node::Fetch { input, .. } => ordered(plan, input),
        _ => false,
    }
}

/// What a filter over a Parquet scan can tell that scan before it opens anything.
///
/// A row group carries the smallest and largest value of each of its columns in the footer, so a
/// conjunct comparing one of those columns against a constant can rule a whole group out without
/// reading a page of it. This pulls out the conjuncts of that shape and drops everything else,
/// which is the conservative direction: a test that is not here costs time, a test that is here
/// wrongly costs rows.
///
/// Only an `AND` is walked into. Under an `OR` a conjunct being false says nothing about the row,
/// and a `NOT` is already gone by the time the plan is bound. Only `read_parquet` is worth doing
/// this for, because a CSV has no footer to read, and only a comparison against this scan's own
/// columns counts, since a binding into some other operator's output is not in this file at all.
fn bounds(plan: &Plan, input: NodeRef, predicate: ExprRef) -> Vec<(usize, Op, Bound)> {
    let Node::TableFunction { index, function, .. } = *plan.node(input) else { return Vec::new() };
    if TableFunction::lookup(plan.string(function)) != Some(TableFunction::ReadParquet) {
        return Vec::new();
    }
    let mut tests = Vec::new();
    conjuncts(plan, predicate, index, &mut tests);
    tests
}

/// Every conjunct of `predicate` that reads as a bounds test, appended to `out`.
fn conjuncts(plan: &Plan, predicate: ExprRef, index: u32, out: &mut Vec<(usize, Op, Bound)>) {
    match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            for child in plan.expr_list(children) {
                conjuncts(plan, *child, index, out);
            }
        }
        Expr::Compare { op, left, right } => {
            if let Some(test) = comparison(plan, op, left, right, index) {
                out.push(test);
            }
        }
        _ => {}
    }
}

/// One comparison read as a test on a column of the scan numbered `index`, if it is one.
///
/// Written either way round, because `5 < a` and `a > 5` say the same thing and the optimizer does
/// not normalise which side the constant sits on. The comparisons that survive a null are the four
/// orderings and equality: `<>` rules out a row group only when the group holds one distinct value,
/// which the footer does not say, and the two distinctness operators are about nulls rather than
/// about bounds.
fn comparison(
    plan: &Plan,
    op: CompareOp,
    left: ExprRef,
    right: ExprRef,
    index: u32,
) -> Option<(usize, Op, Bound)> {
    let op = match op {
        CompareOp::Equal => Op::Equal,
        CompareOp::Less => Op::Less,
        CompareOp::LessOrEqual => Op::LessOrEqual,
        CompareOp::Greater => Op::Greater,
        CompareOp::GreaterOrEqual => Op::GreaterOrEqual,
        CompareOp::NotEqual | CompareOp::DistinctFrom | CompareOp::NotDistinctFrom => return None,
    };
    let (op, binding, value) = match (plan.expr(left), plan.expr(right)) {
        (Expr::Column(binding), Expr::Constant(value)) => (op, *binding, *value),
        (Expr::Constant(value), Expr::Column(binding)) => (op.flipped(), *binding, *value),
        _ => return None,
    };
    if binding.table != index {
        return None;
    }
    Some((binding.column as usize, op, Bound::of_value(plan.value(value))?))
}

/// A pipeline being built from the bottom up.
///
/// It is not a [`Pipeline`] yet because it has no sink. What ends it is whichever node above it
/// turns out to be a pipeline breaker, or the root of the plan, and neither is known until the walk
/// gets back there.
struct Segment<'a> {
    source: Arc<dyn Source + 'a>,
    /// In the order they run, nearest the source first.
    streams: Vec<Arc<dyn DynStream + 'a>>,
    /// What the segment produces as it stands, which changes as streams are added.
    schema: Schema,
    /// The pipelines this one cannot start before.
    after: Vec<PipelineRef>,
}

impl<'a> Segment<'a> {
    /// A segment that is just its source.
    fn new(source: Arc<dyn Source + 'a>, schema: Schema) -> Self {
        Self { source, streams: Vec::new(), schema, after: Vec::new() }
    }

    /// A segment reading what a pipeline breaker finalised into.
    fn reading(source: Arc<dyn Source + 'a>, schema: Schema, after: PipelineRef) -> Self {
        Self { source, streams: Vec::new(), schema, after: vec![after] }
    }

    /// Puts a streaming operator on the end, which becomes what the segment produces.
    fn then(mut self, stream: Arc<dyn DynStream + 'a>, schema: Schema) -> Self {
        self.streams.push(stream);
        self.schema = schema;
        self
    }
}

/// What the walk down the plan carries with it.
struct Building<'a, 'b> {
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &'b Cancel,
    memory: &'b Memory,
    seams: &'b Settings,
    report: &'b Report,
    shape: Shape,
    /// The pipelines closed so far, in the order they have to run.
    done: Vec<Pipeline<'a>>,
    /// One per entry of `done`, in the same order.
    drivers: Vec<Arc<Driver>>,
    /// The bounds tests the filter arm worked out for the scan it is about to walk into.
    ///
    /// A scan is built before the filter above it, because the filter needs the schema the scan
    /// produces, so by the time there is a filter to read there is already a scan that cannot be
    /// told anything. This carries the tests the other way, down the one step from a filter to its
    /// own input, and the scan arm takes them. It is empty every other time it is read, and empty
    /// means hand out every row group, which is what every scan did before pruning existed.
    pruning: Vec<(usize, Op, Bound)>,
}

impl<'a> Building<'a, '_> {
    /// The id of the operator holding the side of this node that has to finish first.
    ///
    /// # Panics
    ///
    /// If the node has one input, which is a node whose arm below should not have called this.
    fn gathered(&self, node: NodeRef) -> u32 {
        self.shape.gathered(node).expect("a node with two inputs has a second operator")
    }

    /// Ends a segment with a sink and puts the finished pipeline on the list.
    fn close(&mut self, segment: Segment<'a>, id: PipelineRef, sink: Arc<dyn DynSink + 'a>) {
        let mut pipeline = Pipeline::new(PipelineId(id), segment.source, sink);
        for stream in segment.streams {
            pipeline = pipeline.then(stream);
        }
        for after in segment.after {
            pipeline = pipeline.after(PipelineId(after));
        }
        self.done.push(pipeline);
        self.drivers.push(self.report.driving(id));
    }

    /// The counters for one operator, registered with the report.
    ///
    /// The row records what this operator picked at each seam it sits on, which is `seams_of` on
    /// its plan node crossed with what is registered and what the statement pinned. That is the
    /// same three things `EXPLAIN` puts its reference marker from, and it is read here rather than
    /// asserted here for a reason worth writing down: this used to mark every operator as a
    /// reference implementation unconditionally, so every ClickBench run said 41 of 41 operators
    /// ran the slow path no matter what had actually run, and the fold that reported it was read as
    /// if it meant something.
    ///
    /// An operator that sits on no registered seam records nothing and stays marked as a reference,
    /// because there is one implementation of it and that one is the obvious correct one. The
    /// marker comes off by itself on the day a seam under it has something else registered and
    /// chosen, with nothing to remember to change here.
    fn watch(
        &self,
        node: NodeRef,
        id: u32,
        pipeline: u32,
        kind: &str,
        detail: Option<&str>,
    ) -> Arc<Counters> {
        let mut counters = Counters::new(id, pipeline, kind);
        if let Some(detail) = detail {
            counters = counters.detailed(detail);
        }
        for seam in seams_of(self.plan.node(node)) {
            if let Some(running) = registries().running(*seam, self.seams) {
                counters = counters.chose(seam.name(), &running.name, running.is_reference);
            }
        }
        self.report.watch(counters)
    }

    fn aggregate(
        &mut self,
        reference: NodeRef,
        input: NodeRef,
        index: u32,
        groups: Slice,
        aggregates: Slice,
        max_groups: Option<usize>,
    ) -> Result<Segment<'a>> {
        let below = self.node(input)?;
        let (aggregate, out) =
            Aggregate::new(self.plan, &below.schema, index, groups, aggregates, self.memory)?;
        let aggregate = match max_groups {
            Some(limit) => aggregate.limit_groups(limit),
            None => aggregate,
        };
        let schema = aggregate.schema().clone();
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        let counters = self.watch(reference, id, pipeline, "Aggregate", None);
        let reading = Arc::clone(&counters);
        self.close(below, pipeline, Arc::new(Watched::new(aggregate, counters)));
        Ok(Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline))
    }

    /// The segment a node produces, closing any pipeline that ends underneath it.
    fn node(&mut self, reference: NodeRef) -> Result<Segment<'a>> {
        let plan = self.plan;
        let memory = self.memory;
        let id = self.shape.operator(reference);
        let pipeline = self.shape.pipeline(reference);
        let segment = match *plan.node(reference) {
            Node::Get { catalog: database, schema, table, index, columns, .. } => {
                let name = QualifiedName::new(
                    plan.string(database),
                    plan.string(schema),
                    plan.string(table),
                );
                let scan = Scan::new(plan, self.catalog.table(&name)?, index, columns)?;
                let schema = scan.schema().clone();
                let counters =
                    self.watch(reference, id, pipeline, "Scan", Some(plan.string(table)));
                Segment::new(Arc::new(Watched::new(scan, counters)), schema)
            }
            Node::Dummy => {
                let dummy = Dummy::new();
                let schema = dummy.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Dummy", None);
                Segment::new(Arc::new(Watched::new(dummy, counters)), schema)
            }
            Node::Values { index, columns, rows } => {
                let values = Values::new(plan, index, columns, rows)?;
                let schema = values.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Values", None);
                Segment::new(Arc::new(Watched::new(values, counters)), schema)
            }
            Node::TableFunction { index, function, args, options, settings, columns } => {
                let name = plan.string(function);
                match TableFunction::lookup(name) {
                    Some(function @ (TableFunction::ReadParquet | TableFunction::ReadCsv)) => {
                        let counters = self.watch(reference, id, pipeline, "FileScan", Some(name));
                        let tests = std::mem::take(&mut self.pruning);
                        let scan = FileScan::new(
                            plan, index, function, args, options, settings, columns, tests,
                        )?
                        .watched(counters.clone());
                        let schema = scan.schema().clone();
                        Segment::new(Arc::new(Watched::new(scan, counters)), schema)
                    }
                    Some(
                        function @ (TableFunction::RudbStrategies | TableFunction::DuckdbKeywords),
                    ) => {
                        let table = match function {
                            TableFunction::DuckdbKeywords => keywords(plan, index, columns)?,
                            _ => strategies(plan, index, columns)?,
                        };
                        let schema = table.schema().clone();
                        // `EXPLAIN` names the table rather than the operator, because every one of
                        // these is the same operator and a plan that said `Metadata` four times
                        // would not say which four tables it read.
                        let counters =
                            self.watch(reference, id, pipeline, "Metadata", Some(function.name()));
                        Segment::new(Arc::new(Watched::new(table, counters)), schema)
                    }
                    _ => {
                        let series = Series::new(plan, index, name, args)?;
                        let schema = series.schema().clone();
                        let counters = self.watch(reference, id, pipeline, "Series", Some(name));
                        Segment::new(Arc::new(Watched::new(series, counters)), schema)
                    }
                }
            }
            Node::Fetch { input, index, args, columns, row } => {
                let below = self.node(input)?;
                let counters = self.watch(reference, id, pipeline, "Fetch", None);
                let fetch = Fetch::new(plan, &below.schema, index, args, columns, row)?
                    .watched(counters.clone());
                let schema = fetch.schema().clone();
                below.then(Arc::new(Watched::new(fetch, counters)), schema)
            }
            Node::Filter { input, predicate } => {
                self.pruning = bounds(plan, input, predicate);
                let below = self.node(input)?;
                // Cleared whether or not the scan arm took them, because a filter over anything
                // else leaves them sitting there for whatever scan the walk reaches next.
                self.pruning = Vec::new();
                let schema = below.schema.clone();
                let filter = Filter::new(plan, reference, predicate, &schema, self.seams)?;
                let counters = self.watch(reference, id, pipeline, "Filter", None);
                below.then(Arc::new(Watched::new(filter, counters)), schema)
            }
            Node::Project { input, index, exprs, names } => {
                let below = self.node(input)?;
                let project = Project::new(plan, &below.schema, index, exprs, names)?;
                let schema = project.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Project", None);
                below.then(Arc::new(Watched::new(project, counters)), schema)
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                self.aggregate(reference, input, index, groups, aggregates, None)?
            }
            Node::Sort { input, keys } => {
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (sort, out) = Sort::new(plan, &schema, keys, memory)?;
                let counters = self.watch(reference, id, pipeline, "Sort", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(sort, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::Limit { input, count, offset } => {
                let max_groups = count
                    .and_then(|count| count.checked_add(offset))
                    .and_then(|count| usize::try_from(count).ok());
                let below = match (plan.node(input).clone(), max_groups) {
                    (
                        Node::Aggregate { input: under, index, groups, aggregates },
                        Some(max_groups),
                    ) => {
                        self.aggregate(input, under, index, groups, aggregates, Some(max_groups))?
                    }
                    _ => self.node(input)?,
                };
                let schema = below.schema.clone();
                let limit = Limit::new(count, offset);
                let counters = self.watch(reference, id, pipeline, "Limit", None);
                below.then(Arc::new(Watched::new(limit, counters)), schema)
            }
            Node::TopN { input, keys, count, offset } => {
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (top, out) = TopN::new(plan, &schema, keys, count, offset, memory)?;
                let counters = self.watch(reference, id, pipeline, "TopN", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(top, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::Distinct { input, on } => {
                let below = self.node(input)?;
                let schema = below.schema.clone();
                let (distinct, out) = Distinct::new(plan, &schema, on, memory)?;
                let counters = self.watch(reference, id, pipeline, "Distinct", None);
                let reading = Arc::clone(&counters);
                self.close(below, pipeline, Arc::new(Watched::new(distinct, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
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
                let right_schema = right.schema.clone();
                let (gather, gathered) = Gather::new(memory);
                let kept = self.watch(reference, gather_id, gathering, "Gather", None);
                self.close(right, gathering, Arc::new(Watched::new(gather, kept)));
                let mut left = self.node(left)?;
                let side = Gathered { schema: &right_schema, rows: gathered };
                let (join, out) =
                    Join::new(plan, &left.schema, side, kind, conditions, self.cancel, memory);
                let schema = join.schema().clone();
                let counters = self.watch(reference, id, pipeline, "Join", None);
                let reading = Arc::clone(&counters);
                left.after.push(gathering);
                self.close(left, pipeline, Arc::new(Watched::new(join, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
            Node::CrossProduct { left, right } => {
                // The right side runs first and is kept as the chunks it arrived in, because it is
                // replayed once per left row. The left side streams, which is the whole point of
                // this operator: the product is produced a chunk at a time and never held, so the
                // product stays in the pipeline the left rows came from rather than starting one.
                let keep_id = self.gathered(reference);
                let aside = self.shape.pipeline(right);
                let right = self.node(right)?;
                let right_schema = right.schema.clone();
                let (keep, kept) = Keep::new(memory);
                let held = self.watch(reference, keep_id, aside, "Keep", None);
                self.close(right, aside, Arc::new(Watched::new(keep, held)));
                let mut left = self.node(left)?;
                let cross = CrossProduct::new(&left.schema, &right_schema, kept);
                let schema = cross.schema().clone();
                let counters = self.watch(reference, id, pipeline, "CrossProduct", None);
                left.after.push(aside);
                left.then(Arc::new(Watched::new(cross, counters)), schema)
            }
            Node::SetOp { left, right, kind, all, index } => {
                // The right side runs first, because nothing can be said about a left row until the
                // whole right side has been counted. That is the dependency edge, spelled out.
                let gather_id = self.gathered(reference);
                let counting = self.shape.pipeline(right);
                let right = self.node(right)?;
                let (gather, gathered) = Gather::new(memory);
                let kept = self.watch(reference, gather_id, counting, "Gather", None);
                self.close(right, counting, Arc::new(Watched::new(gather, kept)));
                let mut left = self.node(left)?;
                let (setop, out) = SetOp::new(&left.schema, gathered, kind, all, index, memory);
                let schema = setop.schema().clone();
                let counters = self.watch(reference, id, pipeline, "SetOp", None);
                let reading = Arc::clone(&counters);
                left.after.push(counting);
                self.close(left, pipeline, Arc::new(Watched::new(setop, counters)));
                Segment::reading(Arc::new(Watched::new(out, reading)), schema, pipeline)
            }
        };
        Ok(segment)
    }
}
