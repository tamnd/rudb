//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Five passes so far. `spec/09-optimizer.md` section 9.1 describes a sequence and [`PASSES`] is
//! the start of it. Column pruning came first, because it is the pass whose absence is measured in
//! gigabytes: a scan that reads 105 columns to answer a question about three is the whole of the
//! difference on ClickBench, and the Parquet reader has been able to read a subset since M1 with
//! nothing able to tell it which subset.

#![forbid(unsafe_code)]

pub mod columns;
pub mod empty;
pub mod filter;
pub mod fold;
pub mod nulls;
pub mod pass;
pub mod tables;
pub mod topn;
mod transitive;
mod walk;

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
///
/// Filter pushdown goes between them. After folding, because a predicate that folds to a constant is
/// a predicate with nothing to push and the pass that moves it should not be the one that finds out.
/// Before pruning, because moving a filter below a projection rewrites it in terms of columns the
/// projection reads, and pruning has to see the plan after the move or it drops a column that
/// something now refers to.
///
/// Empty result pullup is after filter pushdown, because pushdown is what moves an unsatisfiable
/// predicate down to the scan it should stop and what drops the conjuncts that were always true, so
/// the pass that looks for a predicate nothing can satisfy should look after that has happened. It
/// is before pruning for the same reason folding is: the subtrees it removes are subtrees pruning
/// would otherwise walk and work out column lists for.
///
/// Top N is last, because it is the one pass that fuses two operators into one rather than moving
/// something around. Everything before it is written against a sort and a limit, and a pass that had
/// to know about both spellings of the same plan is a pass with two of every rule in it.
pub static PASSES: [&(dyn Pass + Sync); 5] = [
    &fold::ExpressionRewriter,
    &filter::FilterPushdown,
    &empty::EmptyResultPullup,
    &columns::UnusedColumns,
    &topn::TopN,
];

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
/// It also checks, in a debug build, that running the whole sequence a second time changes nothing.
/// That is the property that makes a fixed sequence the right shape: a pass that keeps finding work
/// on a plan it has already rewritten is a pass whose output depends on how many times it happened
/// to run, and in a fixed sequence it runs once, so the plan that reaches the executor is whatever
/// the first pass left behind. Each pass has its own test for this and the assertion is here anyway,
/// because the pair that is not idempotent together is usually a pair that is idempotent apart.
///
/// # Errors
///
/// Whatever a pass reported, and then, in a debug build, if a pass left the plan malformed, narrowed
/// what it returns or did not settle, all three of which are a bug in the pass and not in the query.
pub fn optimize_with(plan: &mut Plan, context: &Context) -> Result<()> {
    run(plan, context, &PASSES)
}

/// The sequence, over a list of passes the tests can choose.
fn run(plan: &mut Plan, context: &Context, passes: &[&(dyn Pass + Sync)]) -> Result<()> {
    let before = output_columns(plan, plan.root());
    once(plan, context, passes)?;
    if cfg!(debug_assertions) {
        plan.validate()?;
        let after = output_columns(plan, plan.root());
        if after != before {
            return Err(Error::internal(format!(
                "a pass turned a query of {before} columns into one of {after}"
            )));
        }
        let settled = plan.to_string();
        once(plan, context, passes)?;
        let again = plan.to_string();
        if again != settled {
            return Err(Error::internal(format!(
                "the passes did not settle, since running them again gave a different plan\n\n{settled}\n{again}"
            )));
        }
    }
    Ok(())
}

/// One run of every pass that is turned on.
fn once(plan: &mut Plan, context: &Context, passes: &[&(dyn Pass + Sync)]) -> Result<()> {
    for pass in passes {
        if context.is_disabled(pass.name()) {
            continue;
        }
        pass.run(plan, context)?;
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
        | Node::TopN { input, .. }
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

    /// A pass that finds the same work every time it looks, which is what the assertion is for.
    #[derive(Debug)]
    #[cfg(debug_assertions)]
    struct Restless;

    #[cfg(debug_assertions)]
    impl Pass for Restless {
        fn name(&self) -> &'static str {
            "restless"
        }

        fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
            let root = plan.root();
            if !matches!(*plan.node(root), Node::Limit { .. }) {
                return Ok(());
            }
            let stacked = plan.add_node(Node::Limit { input: root, count: Some(1), offset: 0 });
            plan.set_root(stacked);
            Ok(())
        }
    }

    /// The settle check is a debug build check, so the test for it is a debug build test. Without
    /// this the release profile job runs a test that asserts an error nothing was going to report,
    /// which is what it had been doing since #196, because the per commit gate runs the tests once
    /// and runs them in debug.
    #[test]
    #[cfg(debug_assertions)]
    fn a_pass_that_never_settles_is_a_reported_error_and_not_a_plan() {
        let text = "Limit 1 offset 0\n  Get memory.main.t AS t #0 [a::INTEGER]\n";
        let mut plan = Plan::parse(text).expect("a well formed plan");
        let error = run(&mut plan, &Context::new(), &[&Restless]).expect_err("it never settles");
        assert!(error.message().starts_with("the passes did not settle"), "{}", error.message());
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
