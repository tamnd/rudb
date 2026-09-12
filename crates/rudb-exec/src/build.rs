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

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, Memory, Result};
use rudb_functions::TableFunction;
use rudb_plan::{Node, NodeRef, Plan};

use crate::adapt::{Broken, Fed, Paired, Streamed};
use crate::cancel::Guarded;
use crate::gather::{Gather, Keep};
use crate::group::{Aggregate, Distinct};
use crate::join::{CrossProduct, Gathered, Join};
use crate::operator::Operator;
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
/// # Errors
///
/// The same as [`build`].
pub fn build_with<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
) -> Result<Box<dyn Operator + 'a>> {
    node(plan, catalog, cancel, memory, plan.root())
}

fn node<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    cancel: &Cancel,
    memory: &Memory,
    reference: NodeRef,
) -> Result<Box<dyn Operator + 'a>> {
    let inner: Box<dyn Operator + 'a> = match *plan.node(reference) {
        Node::Get { catalog: database, schema, table, index, columns, .. } => {
            let name =
                QualifiedName::new(plan.string(database), plan.string(schema), plan.string(table));
            Box::new(Scan::new(plan, catalog.table(&name)?, index, columns)?)
        }
        Node::Dummy => Box::new(Dummy::new()),
        Node::Values { index, columns, rows } => Box::new(Values::new(plan, index, columns, rows)?),
        Node::TableFunction { index, function, args, options, settings, columns } => {
            match TableFunction::lookup(plan.string(function)) {
                Some(function @ (TableFunction::ReadParquet | TableFunction::ReadCsv)) => Box::new(
                    FileScan::new(plan, index, function, args, options, settings, columns)?,
                ),
                Some(TableFunction::RudbStrategies) => {
                    Box::new(Strategies::new(plan, index, columns)?)
                }
                _ => Box::new(Series::new(plan, index, plan.string(function), args)?),
            }
        }
        Node::Filter { input, predicate } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            let filter = Filter::new(plan, predicate, &schema)?;
            Box::new(Streamed::new(input, filter, schema))
        }
        Node::Project { input, index, exprs, names } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let project = Project::new(plan, input.schema(), index, exprs, names)?;
            let schema = project.schema().clone();
            Box::new(Streamed::new(input, project, schema))
        }
        Node::Aggregate { input, index, groups, aggregates } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let (aggregate, out) =
                Aggregate::new(plan, input.schema(), index, groups, aggregates, memory)?;
            let schema = aggregate.schema().clone();
            Box::new(Broken::new(input, aggregate, out, schema))
        }
        Node::Sort { input, keys } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            let (sort, out) = Sort::new(plan, &schema, keys, memory)?;
            Box::new(Broken::new(input, sort, out, schema))
        }
        Node::Limit { input, count, offset } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            Box::new(Streamed::new(input, Limit::new(count, offset), schema))
        }
        Node::TopN { input, keys, count, offset } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            let (top, out) = TopN::new(plan, &schema, keys, count, offset, memory)?;
            Box::new(Broken::new(input, top, out, schema))
        }
        Node::Distinct { input, on } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            let (distinct, out) = Distinct::new(plan, &schema, on, memory)?;
            Box::new(Broken::new(input, distinct, out, schema))
        }
        Node::Join { left, right, kind, conditions } => {
            let left = node(plan, catalog, cancel, memory, left)?;
            let right = node(plan, catalog, cancel, memory, right)?;
            // The right side runs first, because no left row can be answered until every right row
            // it might match has been seen. That is the dependency edge, and it is the same one the
            // hash join builds on.
            let (gather, gathered) = Gather::new(memory);
            let side = Gathered { schema: right.schema(), rows: gathered };
            let (join, out) =
                Join::new(plan, left.schema(), side, kind, conditions, cancel, memory);
            let schema = join.schema().clone();
            Box::new(Paired::new(right, gather, left, join, out, schema))
        }
        Node::CrossProduct { left, right } => {
            let left = node(plan, catalog, cancel, memory, left)?;
            let right = node(plan, catalog, cancel, memory, right)?;
            // The right side runs first and is kept as the chunks it arrived in, because it is
            // replayed once per left row. The left side streams, which is the whole point of this
            // operator: the product is produced a chunk at a time and never held.
            let (keep, kept) = Keep::new(memory);
            let cross = CrossProduct::new(left.schema(), right.schema(), kept);
            let schema = cross.schema().clone();
            Box::new(Fed::new(right, keep, Streamed::new(left, cross, schema)))
        }
        Node::SetOp { left, right, kind, all, index } => {
            let left = node(plan, catalog, cancel, memory, left)?;
            let right = node(plan, catalog, cancel, memory, right)?;
            // The right side runs first, because nothing can be said about a left row until the
            // whole right side has been counted. That is the dependency edge, spelled out.
            let (gather, gathered) = Gather::new(memory);
            let (setop, out) = SetOp::new(left.schema(), gathered, kind, all, index, memory);
            let schema = setop.schema().clone();
            Box::new(Paired::new(right, gather, left, setop, out, schema))
        }
    };
    Ok(Box::new(Guarded::new(inner, cancel.clone())))
}
