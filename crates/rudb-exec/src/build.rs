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
use rudb_common::Result;
use rudb_functions::TableFunction;
use rudb_plan::{Node, NodeRef, Plan};

use crate::group::{Aggregate, Distinct};
use crate::join::{CrossProduct, Join};
use crate::operator::Operator;
use crate::setop::SetOp;
use crate::sort::Sort;
use crate::source::{CsvScan, Dummy, ParquetScan, Scan, Series, Values};
use crate::stream::{Filter, Limit, Project};

/// Builds the operator tree for a plan's root.
///
/// # Errors
///
/// If the plan names a table or a column the catalog does not have, if an expression is malformed
/// in a way [`Plan::validate`] would have caught, or anything an operator's construction reports.
pub fn build<'a>(plan: &'a Plan, catalog: &'a Catalog) -> Result<Box<dyn Operator + 'a>> {
    node(plan, catalog, plan.root())
}

fn node<'a>(
    plan: &'a Plan,
    catalog: &'a Catalog,
    reference: NodeRef,
) -> Result<Box<dyn Operator + 'a>> {
    Ok(match *plan.node(reference) {
        Node::Get { catalog: database, schema, table, index, columns, .. } => {
            let name =
                QualifiedName::new(plan.string(database), plan.string(schema), plan.string(table));
            Box::new(Scan::new(plan, catalog.table(&name)?, index, columns)?)
        }
        Node::Dummy => Box::new(Dummy::new()),
        Node::Values { index, columns, rows } => Box::new(Values::new(plan, index, columns, rows)?),
        Node::TableFunction { index, function, args, columns } => {
            match TableFunction::lookup(plan.string(function)) {
                Some(TableFunction::ReadParquet) => {
                    Box::new(ParquetScan::new(plan, index, args, columns)?)
                }
                Some(TableFunction::ReadCsv) => Box::new(CsvScan::new(plan, index, args, columns)?),
                _ => Box::new(Series::new(plan, index, plan.string(function), args)?),
            }
        }
        Node::Filter { input, predicate } => {
            Box::new(Filter::new(plan, node(plan, catalog, input)?, predicate)?)
        }
        Node::Project { input, index, exprs, names } => {
            Box::new(Project::new(plan, node(plan, catalog, input)?, index, exprs, names)?)
        }
        Node::Aggregate { input, index, groups, aggregates } => {
            Box::new(Aggregate::new(plan, node(plan, catalog, input)?, index, groups, aggregates)?)
        }
        Node::Sort { input, keys } => Box::new(Sort::new(plan, node(plan, catalog, input)?, keys)),
        Node::Limit { input, count, offset } => {
            Box::new(Limit::new(node(plan, catalog, input)?, count, offset))
        }
        Node::Distinct { input, on } => {
            Box::new(Distinct::new(plan, node(plan, catalog, input)?, on))
        }
        Node::Join { left, right, kind, conditions } => Box::new(Join::new(
            plan,
            node(plan, catalog, left)?,
            node(plan, catalog, right)?,
            kind,
            conditions,
        )),
        Node::CrossProduct { left, right } => {
            Box::new(CrossProduct::new(node(plan, catalog, left)?, node(plan, catalog, right)?))
        }
        Node::SetOp { left, right, kind, all, index } => Box::new(SetOp::new(
            node(plan, catalog, left)?,
            node(plan, catalog, right)?,
            kind,
            all,
            index,
        )),
    })
}
