//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! The whole interface is [`optimize`], a plan in and a plan out. Everything else here is the
//! passes it runs and the walks they share.
//!
//! # Why projection pushdown is the first one
//!
//! Not because it is worth the most. `spec/09-optimizer.md` section 9.2 lists filter pushdown and
//! join ordering as the rewrites that decide whether a query finishes, and this one only decides
//! how much of a file gets read. It is first because it changes no operator, its correctness
//! condition is one sentence, and it is the safest place to put the shape of a pass and the tests
//! that check one.
//!
//! It is also, today, the difference between a benchmark number and no benchmark number. Reading a
//! ClickBench partition means reading 105 columns of which a query uses two or three, and before
//! this pass every query read all of them, including `SELECT count(*)`.
//!
//! # What a pass has to promise
//!
//! Two things, and they are checked rather than trusted. In a debug build [`optimize`] validates
//! the plan after every pass, so a pass that leaves a dangling reference fails where it happened
//! rather than in whatever runs next. And it checks the output schema against the input's, because
//! a rewrite that changes what a query returns is the one failure that no amount of running the
//! query afterwards would notice.
//!
//! There is no pass toggle yet. `SET disabled_optimizers` is in #102 with the second pass, because
//! a setting that can turn off one pass is a setting nobody can test against until there are two.

#![forbid(unsafe_code)]

mod projection;
mod walk;

use rudb_common::Result;
use rudb_plan::{Node, NodeRef, Plan};

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 11;

/// A plan rewritten into a plan that answers the same query and reads less to do it.
///
/// The plan handed in is not changed. A caller holding a bound plan keeps it, which is what makes
/// the optimizer's output diffable against its input and what lets a test print both.
///
/// # Errors
///
/// If a pass fails, which means the plan it was given does not hold together.
///
/// # Panics
///
/// In a debug build, if a pass leaves the plan invalid or changes the query's result columns.
pub fn optimize(plan: &Plan) -> Result<Plan> {
    let before = output_columns(plan, plan.root());
    let mut out = plan.clone();
    projection::push_down(&mut out)?;
    debug_assert!(out.validate().is_ok(), "projection pushdown left the plan invalid");
    debug_assert_eq!(
        output_columns(&out, out.root()),
        before,
        "projection pushdown changed what the query returns"
    );
    Ok(out)
}

/// How many columns a node produces, which no pass is allowed to change at the root.
///
/// The count rather than the names and types, because the root of every plan the binder builds is a
/// projection and the check that matters is that a pass did not add or drop one of its expressions.
///
/// The recursion is over the nodes that pass their input's width through, and it is depth first on
/// a plan the optimizer just built, so it is bounded by the same nesting the binder already walked.
fn output_columns(plan: &Plan, reference: NodeRef) -> usize {
    match *plan.node(reference) {
        Node::Get { columns, .. }
        | Node::Values { columns, .. }
        | Node::TableFunction { columns, .. } => plan.field_list(columns).len(),
        Node::Project { exprs, .. } => plan.expr_list(exprs).len(),
        Node::Aggregate { groups, aggregates, .. } => {
            plan.expr_list(groups).len() + plan.expr_list(aggregates).len()
        }
        Node::Dummy => 0,
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::Distinct { input, .. } => output_columns(plan, input),
        // A set operation is as wide as either side, since the binder already required both to
        // agree. A join and a cross product are as wide as the two together.
        Node::SetOp { left, .. } => output_columns(plan, left),
        Node::Join { left, right, .. } | Node::CrossProduct { left, right } => {
            output_columns(plan, left) + output_columns(plan, right)
        }
    }
}
