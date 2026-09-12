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

use crate::adapt::Streamed;
use crate::cancel::Guarded;
use crate::group::{Aggregate, Distinct};
use crate::join::{CrossProduct, Join};
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
        Node::Aggregate { input, index, groups, aggregates } => Box::new(Aggregate::new(
            plan,
            node(plan, catalog, cancel, memory, input)?,
            index,
            groups,
            aggregates,
            memory,
        )?),
        Node::Sort { input, keys } => {
            Box::new(Sort::new(plan, node(plan, catalog, cancel, memory, input)?, keys, memory))
        }
        Node::Limit { input, count, offset } => {
            let input = node(plan, catalog, cancel, memory, input)?;
            let schema = input.schema().clone();
            Box::new(Streamed::new(input, Limit::new(count, offset), schema))
        }
        Node::TopN { input, keys, count, offset } => Box::new(TopN::new(
            plan,
            node(plan, catalog, cancel, memory, input)?,
            keys,
            count,
            offset,
            memory,
        )),
        Node::Distinct { input, on } => {
            Box::new(Distinct::new(plan, node(plan, catalog, cancel, memory, input)?, on, memory))
        }
        Node::Join { left, right, kind, conditions } => Box::new(Join::new(
            plan,
            node(plan, catalog, cancel, memory, left)?,
            node(plan, catalog, cancel, memory, right)?,
            kind,
            conditions,
            cancel,
            memory,
        )),
        Node::CrossProduct { left, right } => Box::new(CrossProduct::new(
            node(plan, catalog, cancel, memory, left)?,
            node(plan, catalog, cancel, memory, right)?,
            memory,
        )),
        Node::SetOp { left, right, kind, all, index } => Box::new(SetOp::new(
            node(plan, catalog, cancel, memory, left)?,
            node(plan, catalog, cancel, memory, right)?,
            kind,
            all,
            index,
            memory,
        )),
    };
    Ok(Box::new(Guarded::new(inner, cancel.clone())))
}
