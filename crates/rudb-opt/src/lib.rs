//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Two passes so far. `spec/09-optimizer.md` section 9.1 describes a sequence and [`PASSES`] is the
//! start of it. Column pruning came first, because it is the pass whose absence is measured in
//! gigabytes: a scan that reads 105 columns to answer a question about three is the whole of the
//! difference on ClickBench, and the Parquet reader has been able to read a subset since M1 with
//! nothing able to tell it which subset.

#![forbid(unsafe_code)]

pub mod columns;
pub mod fold;
pub mod pass;

use rudb_common::{Error, Result};
use rudb_plan::{Node, NodeRef, Plan};

use crate::pass::{Context, Pass};

/// The crate this rank belongs to, so that the layer check has something to read.
pub const RANK: u8 = 11;

/// The passes, in the order they run.
///
/// A fixed sequence rather than a loop to a fixed point, which is what `spec/09-optimizer.md`
/// section 9.1 asks for and what DuckDB does. A fixed point is easy to write and hard to bound: a
/// pair of passes that undo each other runs forever, and the version that stops after a few rounds
/// has a plan that depends on how many rounds it was given.
///
/// Folding is before pruning because folding removes column references and pruning drops the columns
/// nothing refers to, so a `CASE WHEN false THEN t.a ELSE 1 END` costs a column read when the two run
/// the other way around. Nothing in the other direction is given up: pruning drops columns and
/// renumbers bindings, and neither of those makes anything foldable.
pub static PASSES: [&(dyn Pass + Sync); 2] = [&fold::ExpressionRewriter, &columns::UnusedColumns];

/// Rewrites a bound plan into the plan that runs, with every pass on.
///
/// # Errors
///
/// If a pass left the plan malformed or narrowed what it returns, which is a bug in the pass and
/// not in the query.
pub fn optimize(plan: &mut Plan) -> Result<()> {
    optimize_with(plan, &Context::new())
}

/// Rewrites a bound plan into the plan that runs, skipping the passes the context turned off.
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
/// Whatever a pass reported, and then, in a debug build, if a pass left the plan malformed or
/// narrowed what it returns, which is a bug in the pass and not in the query.
pub fn optimize_with(plan: &mut Plan, context: &Context) -> Result<()> {
    let before = output_columns(plan, plan.root());
    for pass in PASSES {
        if context.is_disabled(pass.name()) {
            continue;
        }
        pass.run(plan, context)?;
    }
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

    #[test]
    fn no_two_passes_answer_to_the_same_name() {
        // The name is the address, so two passes sharing one would make the toggle turn off
        // whichever came first in the list and silently leave the other on.
        let mut names: Vec<&str> = PASSES.iter().map(|pass| pass.name()).collect();
        names.sort_unstable();
        let held = names.len();
        names.dedup();
        assert_eq!(names.len(), held, "{names:?}");
    }

    #[test]
    fn a_pass_that_is_turned_off_does_not_run() {
        let text = "Project #1 [\"+\"(1::INTEGER, 1::INTEGER)::INTEGER AS n]\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        let mut plan = Plan::parse(text).expect("a well formed plan");
        let context = Context::without("expression_rewriter").expect("a name that is a pass");
        optimize_with(&mut plan, &context).expect("the other pass still runs");
        assert_eq!(
            plan.to_string(),
            "Project #1 [\"+\"(1::INTEGER, 1::INTEGER)::INTEGER AS n]\n  Get memory.main.t AS t #0 []\n"
        );
    }

    /// Folding before pruning, which is the reason the order in [`PASSES`] is the order it is. The
    /// column is read only by a branch that cannot be taken, so one pass has to remove the branch
    /// before the other can see that nothing reads the column.
    #[test]
    fn folding_runs_first_so_that_pruning_sees_the_columns_it_freed() {
        let text = "Project #1 [CASE WHEN FALSE::BOOLEAN THEN #0.1::INTEGER ELSE #0.0::INTEGER END::INTEGER AS n]\n  Get memory.main.t AS t #0 [a::INTEGER, b::INTEGER]\n";
        assert_eq!(
            optimized(text),
            "Project #1 [#0.0::INTEGER AS n]\n  Get memory.main.t AS t #0 [a::INTEGER]\n"
        );
    }
}
