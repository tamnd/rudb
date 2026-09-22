//! The rewrite passes, cardinality estimation, join ordering, predicate transfer and layout adaptation.
//!
//! Rank 11 in the layer rule. See `xtask/layers.toml` and `spec/18-package-layout.md`.
//!
//! Twenty one passes so far. `spec/09-optimizer.md` section 9.1 describes a sequence and [`PASSES`]
//! is the start of it. Column pruning came first, because it is the pass whose absence is measured
//! in gigabytes: a scan that reads 105 columns to answer a question about three is the whole of the
//! difference on ClickBench, and the Parquet reader has been able to read a subset since M1 with
//! nothing able to tell it which subset.

#![forbid(unsafe_code)]

pub mod bounds;
pub mod columns;
pub mod cte;
pub mod delim;
pub mod dependent;
pub mod distinct;
mod domain;
pub mod empty;
pub mod estimate;
pub mod explain;
pub mod extremes;
pub mod filter;
pub mod fold;
pub mod fromkey;
pub mod keys;
pub mod late;
pub mod limit;
pub mod link;
pub mod nulls;
pub mod order;
pub mod pass;
pub mod presize;
pub mod semi;
pub mod sides;
pub mod tables;
pub mod topn;
mod transitive;
pub mod unnest;
mod walk;

use rudb_common::{Error, Result};
use rudb_plan::{JoinKind, Node, NodeRef, Plan};

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
/// Join ordering is immediately after filter pushdown, because pushdown is what turns the binder's
/// cross products into joins with conditions on them, and which cross products are left over after
/// it has placed every condition it can is the whole of what this pass reads. Running it first would
/// be reading a plan where every join is still a cross product and none of the conditions have been
/// placed, which says nothing about anything. Before everything else after that, because every later
/// pass reads a join as it ends up. The build side is the clearest of those and is chosen last, but
/// empty result pullup, column pruning and late materialisation all walk the join tree and all of
/// them should walk the tree that is going to run.
///
/// The deliminator is between filter pushdown and join ordering, and both halves of that matter.
/// After pushdown, because the shape it reads is a filter over the marker of one single join and
/// before pushdown that test is one conjunct of the query's whole `WHERE`, sitting above however
/// many other joins the `FROM` list turned into. Before join ordering, because what it leaves
/// behind is a semi join where there was a join back to a domain, and the pass that decides which
/// order to build a region in should be reading the joins that are going to run.
///
/// Pushing the keys of a correlated subquery into the aggregate that answers it goes between the
/// deliminator and join ordering, for the two reasons the deliminator is there. After pushdown,
/// because the relation it copies is the one the filters have already been moved into and copying
/// it before they move would copy a whole table. Before join ordering, because the semi join it
/// writes is a join that is going to run and a region the pass reads should be the region the
/// executor gets.
///
/// Empty result pullup is after filter pushdown, because pushdown is what moves an unsatisfiable
/// predicate down to the scan it should stop and what drops the conjuncts that were always true, so
/// the pass that looks for a predicate nothing can satisfy should look after that has happened. It
/// is before pruning for the same reason folding is: the subtrees it removes are subtrees pruning
/// would otherwise walk and work out column lists for.
///
/// Limit pushdown is second to last, which is to say it is immediately before top N. A limit that
/// has moved below the projections above it is a limit that may now be sitting directly on a sort,
/// and that pair is what top N fuses, so running the two the other way around would leave the fusion
/// with a plan it cannot see the shape of.
///
/// The distinct aggregate rewrite is second, ahead of everything that moves an operator around,
/// because it is the one pass that changes what an aggregate is rather than where it sits. Every
/// other pass here is written against a single aggregate node, and running this one ahead of them
/// means none of them has to know that `COUNT(DISTINCT x)` has a second spelling. In particular the
/// limit that fuses into an aggregate has to fuse into the outer one, and after this pass the outer
/// one is the only one it can see.
///
/// What it is not ahead of is folding, and that order is the other way round for a reason the AST
/// fuzz target found. The rewrite fires only when every `DISTINCT` call in a node has the same
/// argument, and whether two arguments are the same is a question folding answers: `max(DISTINCT
/// 1 + 1)` and `min(DISTINCT 2)` are two arguments before it and one after it. With the rewrite
/// first the pass sees the unfolded pair, refuses, and a second run of the sequence over its own
/// output fires, which is the idempotence assertion below failing. Folding has no opinion about
/// either spelling of an aggregate, so nothing is given up by putting it in front.
///
/// Collapsing an aggregate onto its group key is fourth, immediately after the pass that takes
/// dependent expressions out of a group key. Both of them end up with a projection over an
/// aggregate, and the order between them decides how much the second one sees: `GROUP BY c, f(c)` is
/// a two key aggregate until dependent group keys have run and a one key aggregate afterwards, and
/// only the second of those is a shape the collapse applies to. It is also before filter pushdown,
/// because the expressions it leaves in a projection are the ones a `HAVING` should get to run
/// before, and pushdown is what moves the `HAVING` under them.
///
/// Group key pushdown is after the two passes that turn a mark into a semi join, and that is the
/// difference between the pass firing on TPC-H q20 and not. It copies the relation that restricts
/// the outer query so the aggregate underneath builds only the groups that will be read, and the
/// restriction in q20 is `ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%')`.
/// Ahead of the mark rewrites that is a filter over a mark join, which is a shape with a column
/// that exists only to be tested and a copy of which would have to reproduce it; afterwards it is a
/// semi join, which is a shape that copies. Nothing is given up by waiting, because the copy it
/// takes is then of a subtree join order has already ordered, and the semi join it inserts is still
/// in front of the pass that pushes semi joins down.
///
/// Top N is last of the passes that rewrite the shape of a plan, because it is the one that fuses
/// two operators into one rather than moving something around. Everything before it is written
/// against a sort and a limit, and a pass that had to know about both spellings of the same plan is
/// a pass with two of every rule in it.
///
/// The build side is chosen after all of them, and that is not an ordering preference so much as a
/// consequence of what it reads. It picks a side per join from an estimate of how many rows each
/// side produces, and a filter that has not been pushed down yet, a limit that has not reached the
/// scan yet and a subtree that empty result pullup is about to delete are all estimates of a plan
/// nobody is going to run. It is also the only pass here that writes a field rather than moving a
/// node, so nothing after it would have anything to do with what it wrote.
///
/// Dropping an unread materialisation is after the empty result pullup and before everything that
/// moves an operator around. After, because the pullup is what turns a body into an empty relation
/// and a body that has become one reads nothing, so a run that looked before it would find the work
/// on the next run instead, which is the fixed sequence not settling. Before the rest, because the
/// subtree it removes is a subtree they would otherwise walk, and because the operators it leaves
/// next to each other are the pairs limit pushdown and top N are looking for.
///
/// Filter pushdown and limit pushdown used to make work for each other, which is worth recording
/// here because the fix is not in this list. Limit pushdown trades a limit with the projection under
/// it, so `Filter / Limit / Project` became `Filter / Project / Limit`, and the filter that had
/// nowhere to go then had a projection to go through, one position after the pass that would have
/// taken it. Reordering does not help: filter pushdown has to be in front of join order, the mark
/// rewrites and group key pushdown, all of which read a plan whose predicates have already landed,
/// and limit pushdown has to be behind the unread materialisation drop for the reason above. Nor
/// does running either of them twice, because each round the two of them trade moves one more level
/// of a nested query, so the number of rounds it takes is how deeply the query is nested. What fixes
/// it is filter pushdown crossing the limits itself, which is described where it does that.
///
/// Reading a link instead of building a hash table is last of all, after the build side has been
/// chosen. It replaces a join outright, so a pass that ran after it would have to know about a
/// second kind of join to say anything about one, and there is nothing any of them want to say:
/// the join it leaves behind has the same inputs, the same condition and the same kind. It also
/// needs the plan to have stopped moving, because what it asks about the parent is how many rows
/// reach it and what it asks about the child is whether the rows are still the table's own, and
/// both of those are questions about a plan somebody is going to run rather than a draft of one.
/// Running after the build side costs nothing, because the side a link join builds is neither of
/// them.
pub static PASSES: [&(dyn Pass + Sync); 21] = [
    &fold::ExpressionRewriter,
    &distinct::DistinctAggregateRewrite,
    &dependent::DependentGroupKeys,
    &fromkey::AnswersFromTheKey,
    &filter::FilterPushdown,
    &delim::Deliminator,
    &order::JoinOrder,
    &semi::MarkToSemi,
    &semi::DistinctToSemi,
    &keys::GroupKeyPushdown,
    &semi::SemiPushdown,
    &empty::EmptyResultPullup,
    &extremes::StatisticsPropagation,
    &cte::UnusedMaterialization,
    &columns::UnusedColumns,
    &limit::LimitPushdown,
    &topn::TopN,
    &late::LateMaterialization,
    &sides::BuildSideProbeSide,
    &presize::AggregatePresize,
    &link::LinkJoinRewrite,
];

/// Every name `SET disabled_optimizers` accepts, which is every name DuckDB accepts.
///
/// `SELECT name FROM duckdb_optimizers()` on the pinned binary, sorted, all forty four of them.
/// Thirteen of them name a pass [`PASSES`] holds, and every name here is one rudb takes without
/// complaint, because turning off a pass that does not exist is a thing that has already happened.
///
/// Accepting the other thirty one is the whole point. Forty five files in the upstream corpus run a
/// `SET disabled_optimizers`, and most of them name a pass rudb has not written,
/// `compressed_materialization` and `join_elimination` and the rest. Refusing those makes the
/// `SET` fail, and a failed `SET` in a sqllogictest file ends the file, so every record after it
/// goes unasked over a pass whose absence changes no answer.
///
/// The list is written down rather than discovered, because there is nothing to discover it from:
/// DuckDB is a binary that may not be on the machine and this has to answer the same way when it is
/// not. It is pinned to the same commit the rest of the compatibility work is pinned to, and a
/// release that adds a pass adds a name here.
pub static UPSTREAM: [&str; 44] = [
    "aggregate_function_rewriter",
    "aggregate_reuse",
    "build_side_probe_side",
    "column_lifetime",
    "common_aggregate",
    "common_subexpressions",
    "common_subplan",
    "compressed_materialization",
    "cte_filter_pusher",
    "cte_inlining",
    "deliminator",
    "distinct_aggregate_rewrite",
    "duplicate_groups",
    "empty_result_pullup",
    "expression_rewriter",
    "extension",
    "filter_pullup",
    "filter_pushdown",
    "grouping_sets",
    "in_clause",
    "join_elimination",
    "join_filter_pushdown",
    "join_order",
    "late_materialization",
    "limit_pushdown",
    "materialized_cte",
    "outer_join_simplification",
    "partial_aggregate_pushdown",
    "partitioned_execution",
    "projection_pullup",
    "regex_range",
    "remote_pushdown",
    "reorder_filter",
    "row_group_pruner",
    "sampling_pushdown",
    "scalar_fn_pushdown",
    "statistics_propagation",
    "top_n",
    "top_n_window_elimination",
    "type_pushdown",
    "unnest_rewriter",
    "unused_columns",
    "window_rewriter",
    "window_self_join",
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
    unnest::lower(plan)?;
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
        | Node::TableFunction { columns, .. }
        | Node::Fetch { columns, .. }
        | Node::TableFetch { columns, .. }
        | Node::CteScan { columns, .. } => plan.field_list(columns).len(),
        Node::Project { exprs, .. } => plan.expr_list(exprs).len(),
        Node::Aggregate { groups, aggregates, .. } => {
            plan.expr_list(groups).len() + plan.expr_list(aggregates).len()
        }
        Node::Window { input, expressions, .. } => {
            output_columns(plan, input) + plan.expr_list(expressions).len()
        }
        Node::LateralFunction { input, columns, .. } => {
            output_columns(plan, input) + plan.field_list(columns).len()
        }
        Node::Dummy => 0,
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::TopN { input, .. }
        | Node::Distinct { input, .. } => output_columns(plan, input),
        // A materialisation returns what the query that reads it returns. The held columns are not
        // part of that: they go to the reads of it and never past this node.
        Node::MaterializedCte { body, .. } => output_columns(plan, body),
        // A set operation is as wide as either side, since the binder already required the two to
        // agree. A join and a cross product are as wide as the two together.
        Node::SetOp { left, .. } => output_columns(plan, left),
        Node::Join { left, right, .. }
        | Node::DependentJoin { left, right, .. }
        | Node::CrossProduct { left, right } => {
            output_columns(plan, left) + output_columns(plan, right)
        }
        // A semi or anti link join never touches the parent, so its width is the child's. The
        // other two put the gathered parent columns after the child's and are as wide as both.
        Node::LinkJoin { child, parent, kind, .. } => match kind {
            JoinKind::Semi | JoinKind::Anti => output_columns(plan, child),
            _ => output_columns(plan, child) + output_columns(plan, parent),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rudb_catalog::Catalog;
    // The only test below that names a `Bound` builds under `debug_assertions`, so in a release
    // test build this import is unused and `-D warnings` turns that into an error. That is the
    // release job on main since #1092.
    #[cfg(debug_assertions)]
    use rudb_plan::Bound;

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
    fn every_optimizer_allocation_keeps_a_source_span() {
        let sql =
            "SELECT count(DISTINCT x) FROM (VALUES ('a'), ('b'), ('b')) t(x) WHERE true LIMIT 1";
        let mut plan = rudb_bind::bind_sql(sql, &Catalog::new()).expect("the query binds");
        let nodes = plan.node_count();
        let exprs = plan.expr_count();

        optimize(&mut plan).expect("the complete optimizer sequence succeeds");

        assert!(!plan.node_span(plan.root()).is_empty(), "the optimized root keeps a source range");
        for at in nodes..plan.node_count() {
            let at = u32::try_from(at).expect("the plan arena fits in a reference");
            assert!(!plan.node_span(at).is_empty(), "optimizer node {at} has no source range");
        }
        for at in exprs..plan.expr_count() {
            let at = u32::try_from(at).expect("the expression arena fits in a reference");
            assert!(
                !plan.expr_span(at).is_empty(),
                "optimizer expression {at} has no source range"
            );
        }
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
            let stacked = plan.add_node(Node::Limit {
                input: root,
                count: Bound::Rows(1),
                offset: Bound::Rows(0),
            });
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

    /// The pair that made filter pushdown run twice. Limit pushdown trades the limit with the
    /// projection under it, and the filter that was stuck above the limit then has a projection to
    /// go through, which is work the one run of filter pushdown was already past. With one run this
    /// is an `INTERNAL Error: the passes did not settle` on a query anybody could write.
    #[test]
    fn a_filter_over_a_subquery_that_ends_in_a_limit_settles() {
        let text = concat!(
            "Project #2 [#1.0::INTEGER AS x]\n",
            "  Filter (#1.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
            "    Limit 4 offset 0\n",
            "      Project #1 [#0.0::INTEGER AS x]\n",
            "        Get memory.main.t AS t #0 [x::INTEGER]\n",
        );
        assert_eq!(
            optimized(text),
            concat!(
                "Project #2 [#1.0::INTEGER AS x]\n",
                "  Project #1 [#0.0::INTEGER AS x]\n",
                "    Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n",
                "      Limit 4 offset 0\n",
                "        Get memory.main.t AS t #0 [x::INTEGER]\n",
            )
        );
        assert_eq!(width(text), 1);
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

    /// Folding before the distinct aggregate rewrite, which is the other half of that order. The
    /// rewrite wants every `DISTINCT` call in a node to have the same argument, and these two have
    /// the same argument only once folding has run, so with the passes the other way around the
    /// rewrite refuses here and fires on a second run over its own output.
    #[test]
    fn folding_runs_first_so_that_the_distinct_rewrite_sees_one_argument_rather_than_two() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[max(DISTINCT \"+\"(1::INTEGER, 1::INTEGER)::INTEGER)::INTEGER, min(DISTINCT 2::INTEGER)::INTEGER]\n",
            "  Dummy\n",
        );
        assert_eq!(
            optimized(text),
            concat!(
                "Aggregate #1 groups=[] aggregates=[max(#2.0::INTEGER)::INTEGER, min(#2.0::INTEGER)::INTEGER]\n",
                "  Aggregate #2 groups=[2::INTEGER] aggregates=[]\n",
                "    Dummy\n",
            )
        );
    }
}
