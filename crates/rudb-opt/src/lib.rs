//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! One pass so far, which is column pruning. `spec/09-optimizer.md` section 9.1 describes a sequence
//! and this is the first of it, chosen because it is the one whose absence is measured in gigabytes:
//! a scan that reads 105 columns to answer a question about three is the whole of the difference on
//! ClickBench, and the Parquet reader has been able to read a subset since M1 with nothing able to
//! tell it which subset.

#![forbid(unsafe_code)]

pub mod columns;

use rudb_common::{Error, Result};
use rudb_plan::{Node, NodeRef, Plan};

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 11;

/// Rewrites a bound plan into the plan that runs.
///
/// Every pass preserves the plan invariant, which is what [`Plan::validate`] checks, so this checks
/// it once at the end rather than each pass checking itself. In a release build it does not, because
/// a pass that breaks the invariant breaks it the same way in both builds and the debug build is
/// where that gets found.
///
/// It also checks that the plan still returns as many columns as it did on the way in. A malformed
/// plan is found by whatever runs next, but a rewrite that quietly changes what a query returns is
/// the one failure that running the query afterwards would not notice, and column pruning in
/// particular is a pass whose only way of being wrong is exactly that.
///
/// # Errors
///
/// If a pass left the plan malformed or narrowed what it returns, which is a bug in the pass and
/// not in the query.
pub fn optimize(plan: &mut Plan) -> Result<()> {
    let before = output_columns(plan, plan.root());
    columns::prune(plan);
    if cfg!(debug_assertions) {
        plan.validate()?;
        let after = output_columns(plan, plan.root());
        if after != before {
            return Err(Error::internal(format!(
                "a pass turned a query of {before} columns into one of {after}"
            )));
        }
    }
    Ok(())
}

/// How many columns a node produces, which no pass is allowed to change at the root.
///
/// The count rather than the names and types, because the root of a plan the binder builds is a
/// projection and what has to hold is that a pass did not add or drop one of its expressions. The
/// recursion is over the operators that pass their input's width through, so its depth is the
/// nesting the binder already walked to build the plan.
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
        // A set operation is as wide as either side, since the binder already required the two to
        // agree. A join and a cross product are as wide as the two together.
        Node::SetOp { left, .. } => output_columns(plan, left),
        Node::Join { left, right, .. } | Node::CrossProduct { left, right } => {
            output_columns(plan, left) + output_columns(plan, right)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How wide the plan a text prints is, before anything has run over it.
    fn width(text: &str) -> usize {
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        output_columns(&plan, plan.root())
    }

    /// Optimize the plan a text prints and hand back what it printed afterwards.
    fn optimized(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        optimize(&mut plan).unwrap_or_else(|error| panic!("{text} did not optimize: {error}"));
        plan.to_string()
    }

    #[test]
    fn the_width_of_a_plan_is_the_width_of_whatever_produces_its_columns() {
        assert_eq!(
            width(
                "Project #1 [#0.0::INTEGER AS a]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n"
            ),
            1
        );
        assert_eq!(width("Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n"), 2);
        assert_eq!(width("Dummy\n"), 0);
        assert_eq!(
            width(
                "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n  Get memory.main.t AS t #0 [a::INTEGER]\n"
            ),
            2
        );
    }

    /// A `LIMIT` or a `SORT` is as wide as what is under it, which is the recursion this function
    /// exists for and the part a single level check would get wrong.
    #[test]
    fn an_operator_that_passes_its_input_through_is_as_wide_as_its_input() {
        assert_eq!(
            width("Limit 1 offset 0\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n"),
            2
        );
    }

    /// A join is both sides and a set operation is either one, since the binder already required
    /// the two sides of a set operation to agree.
    #[test]
    fn a_join_is_both_sides_together_and_a_set_operation_is_one_of_them() {
        assert_eq!(
            width(
                "Join INNER on=[]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n  Get memory.main.u AS u #1 [x::INTEGER]\n"
            ),
            3
        );
        assert_eq!(
            width(
                "SetOp UNION ALL #2\n  Get memory.main.t AS t #0 [a::INTEGER]\n  Get memory.main.u AS u #1 [x::INTEGER]\n"
            ),
            1
        );
    }

    /// The check is on the whole of `optimize` and not on one pass, so it keeps holding as passes
    /// are added. This is the shape it runs over today.
    #[test]
    fn optimizing_keeps_a_query_as_wide_as_it_was() {
        let before = "Project #1 [#0.1::VARCHAR AS b]\n  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR]\n";
        let after = "Project #1 [#0.0::VARCHAR AS b]\n  Get memory.main.t AS t #0 [b::VARCHAR]\n";
        assert_eq!(optimized(before), after);
        assert_eq!(width(before), width(after));
    }
}
