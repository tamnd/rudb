//! Which input of each join the executor gathers whole.
//!
//! A join is two inputs and a dependency edge between them. One side is run to the end and held,
//! and then the other side's rows arrive and are matched against what was held. [`BuildSide`] says
//! which side that is, the binder emits the same answer for every join because at binding time
//! there is nothing to choose with, and this pass is what chooses.
//!
//! The choice is made from [`estimate::rows`] and nothing else. No sample, no histogram, no
//! reordering: this pass never moves a join and never changes what any join produces. It writes one
//! field, and a plan it has run over is a plan whose answers are the answers it had before.
//!
//! # Which side is the right one
//!
//! Not the one the name suggests. "Build side" is hash join vocabulary, where the rule is to build
//! from the smaller input because the hash table is what has to fit in memory. rudb has no hash
//! join yet: #62 is where it lands, and until then every join is the nested loop in
//! `crates/rudb-exec/src/join.rs`, which has the opposite preference.
//!
//! The nested loop holds the gathered side as chunks and, for each row of the other side, walks
//! every one of those chunks and evaluates the conditions over it. So the number of calls into the
//! vectorized evaluator is the driving side's row count times the gathered side's chunk count, and
//! each call does a chunk's worth of work. Rows of the driving side are paid for one at a time and
//! rows of the gathered side are paid for two thousand at a time, which means the cheap thing to do
//! is to gather the *larger* side and drive from the smaller one.
//!
//! Measured on server3 against the nested loop, two tables joined on a key with no predicate:
//!
//! ```text
//! small(40) driving, big(200_000) gathered        834ms
//! big(200_000) driving, small(40) gathered      1_395ms     1.67x
//! tiny(4) driving, big(400_000) gathered          703ms
//! big(400_000) driving, tiny(4) gathered        3_566ms     5.07x
//! ```
//!
//! The ratio grows with the ratio between the sides, which is what the count above predicts, so
//! this is the shape of the operator rather than a constant factor somewhere.
//!
//! When #62 lands, this rule inverts and `prefers` below is the one function that changes. That is
//! why the flag on the node says *which side is gathered* rather than *which side is smaller*: the
//! first is a fact about the plan that both operators agree on, and the second is a policy that
//! they disagree about. A flag that meant "the small one" would have to be rewritten in every plan
//! in the repository on the day the hash join arrives.
//!
//! # What it leaves alone
//!
//! A join whose kind names a side. Swapping the inputs of a join means running it as its mirror,
//! and [`rudb_plan::JoinKind::mirrored`] is where the kinds that have one are listed. `SEMI`,
//! `ANTI`, `SINGLE` and `MARK` produce the left input's rows, or a column about them, so their left
//! input is the subject rather than a side, and `POSITIONAL` pairs the nth row with the nth row,
//! which nothing about one input alone preserves. Those keep whatever they were given.
//!
//! A join where either estimate is `None`, which is a scan of a table nobody measured and anything
//! above one. Guessing between two sides when one of the two numbers is missing is how an optimizer
//! talks itself into the plan that is five times slower, and the side the binder emitted is at
//! least the side every plan had before this pass existed.
//!
//! A join whose two sides estimate the same. There is nothing to choose between them and choosing
//! anyway would make the flag depend on which comparison operator was written down.

use rudb_plan::{BuildSide, Node, Plan};

use rudb_common::Result;

use crate::estimate::{self, Statistics};
use crate::pass::{Context, Pass, top_down};

/// Sets the build side on every join that has an estimate for both of its inputs.
///
/// DuckDB's name for this is `build_side_probe_side`, which is already in [`crate::UPSTREAM`]
/// because corpus files turn it off, so a file that did so was being answered with silence and is
/// now being answered.
#[derive(Debug)]
pub struct BuildSideProbeSide;

impl Pass for BuildSideProbeSide {
    fn name(&self) -> &'static str {
        "build_side_probe_side"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        choose(plan, context.statistics());
        Ok(())
    }
}

/// Which side the nested loop would rather have gathered, given what each side is guessed to hold.
///
/// The larger one, for the reason in the module documentation: the gathered side is walked a chunk
/// at a time and the driving side a row at a time. `None` when the two are equal, which is not a
/// preference.
#[must_use]
fn prefers(left: u64, right: u64) -> Option<BuildSide> {
    match left.cmp(&right) {
        std::cmp::Ordering::Greater => Some(BuildSide::Left),
        std::cmp::Ordering::Less => Some(BuildSide::Right),
        std::cmp::Ordering::Equal => None,
    }
}

/// Writes the chosen side onto every join in `plan` that has one.
///
/// In place rather than by rebuilding, because this changes no node's shape and no node's children.
/// Rebuilding would give every join above a rewritten join a new reference for no reason, and a
/// pass that moves every node is a pass whose output is hard to read against its input.
///
/// Idempotent by construction: the answer is a function of the two estimates and the kind, none of
/// which this pass touches, so a second run writes what is already there.
fn choose(plan: &mut Plan, stats: &Statistics) {
    for node in top_down(plan) {
        let Node::Join { left, right, kind, .. } = *plan.node(node) else {
            continue;
        };
        if kind.mirrored().is_none() {
            continue;
        }
        let (Some(left), Some(right)) =
            (estimate::rows(plan, left, stats), estimate::rows(plan, right, stats))
        else {
            continue;
        };
        let Some(wanted) = prefers(left, right) else {
            continue;
        };
        if let Node::Join { build, .. } = plan.node_mut(node) {
            *build = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{BuildSide, JoinKind, Node, Plan};

    use super::{BuildSideProbeSide, prefers};
    use crate::estimate::Statistics;
    use crate::pass::{Context, Pass};

    /// A join of two scans, with the row counts named by the caller.
    ///
    /// Written as text and parsed, the way the rest of the optimizer's tests build plans, so that
    /// what the pass is given is the plan a reader of the test can see.
    fn joined(kind: &str, left: u64, right: u64) -> (Plan, Context) {
        let text = format!(
            "Join {kind} on=[]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT]\n"
        );
        let plan =
            Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let mut statistics = Statistics::new();
        statistics.record("memory", "main", "l", left);
        statistics.record("memory", "main", "r", right);
        let mut context = Context::new();
        context.measure(statistics);
        (plan, context)
    }

    /// Runs the pass and reports the side it wrote on the root.
    fn chosen(kind: &str, left: u64, right: u64) -> BuildSide {
        let (mut plan, context) = joined(kind, left, right);
        BuildSideProbeSide.run(&mut plan, &context).expect("the pass does not fail");
        let Node::Join { build, .. } = *plan.node(plan.root()) else {
            panic!("the root stopped being a join");
        };
        build
    }

    #[test]
    fn the_larger_side_is_the_one_gathered_because_the_nested_loop_walks_it_a_chunk_at_a_time() {
        assert_eq!(prefers(100_000, 4), Some(BuildSide::Left));
        assert_eq!(prefers(4, 100_000), Some(BuildSide::Right));
    }

    #[test]
    fn two_sides_of_the_same_size_are_not_a_preference() {
        assert_eq!(prefers(500, 500), None);
    }

    #[test]
    fn a_big_left_and_a_small_right_gathers_the_left() {
        assert_eq!(chosen("INNER", 400_000, 4), BuildSide::Left);
    }

    #[test]
    fn a_small_left_and_a_big_right_keeps_the_side_the_binder_emitted() {
        assert_eq!(chosen("INNER", 4, 400_000), BuildSide::Right);
    }

    /// The mirror of a `LEFT` join is a `RIGHT` join, which the executor knows how to run, so this
    /// one is allowed to swap like an inner join.
    #[test]
    fn an_outer_join_whose_kind_has_a_mirror_still_gets_the_larger_side() {
        assert_eq!(chosen("LEFT", 400_000, 4), BuildSide::Left);
        assert_eq!(chosen("FULL", 400_000, 4), BuildSide::Left);
    }

    /// A semi join produces its left input's rows. There is no join that produces its right
    /// input's rows instead, so there is nothing to swap it into whatever the estimates say.
    #[test]
    fn a_kind_that_names_its_left_input_as_the_subject_is_left_alone() {
        assert_eq!(chosen("SEMI", 400_000, 4), BuildSide::Right);
        assert_eq!(chosen("ANTI", 400_000, 4), BuildSide::Right);
        assert_eq!(chosen("MARK", 400_000, 4), BuildSide::Right);
        assert_eq!(chosen("POSITIONAL", 400_000, 4), BuildSide::Right);
    }

    #[test]
    fn a_table_nobody_measured_leaves_the_join_as_it_was() {
        let text = "Join INNER on=[]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("the plan parses");
        let mut statistics = Statistics::new();
        // Only one of the two sides, which is the case the module documentation calls out: one
        // number is not enough to choose with and a default in place of the other one would be a
        // guess wearing a measurement's name.
        statistics.record("memory", "main", "l", 400_000);
        let mut context = Context::new();
        context.measure(statistics);
        BuildSideProbeSide.run(&mut plan, &context).expect("the pass does not fail");
        let Node::Join { build, .. } = *plan.node(plan.root()) else {
            panic!("the root stopped being a join");
        };
        assert_eq!(build, BuildSide::Right, "one estimate is not two estimates");
    }

    #[test]
    fn running_it_twice_writes_what_is_already_there() {
        let (mut plan, context) = joined("INNER", 400_000, 4);
        BuildSideProbeSide.run(&mut plan, &context).expect("the pass does not fail");
        let once = plan.to_string();
        BuildSideProbeSide.run(&mut plan, &context).expect("the pass does not fail");
        assert_eq!(plan.to_string(), once, "the pass did not settle");
    }

    /// Every kind the mirror table says has no mirror, asked of the mirror table rather than of a
    /// plan, so that a kind added later without a mirror is still covered here.
    #[test]
    fn the_kinds_with_no_mirror_are_the_ones_this_pass_refuses_to_touch() {
        for kind in
            [JoinKind::Semi, JoinKind::Anti, JoinKind::Single, JoinKind::Mark, JoinKind::Positional]
        {
            assert_eq!(kind.mirrored(), None, "{kind:?} grew a mirror and this test did not");
        }
        for kind in [JoinKind::Inner, JoinKind::Left, JoinKind::Right, JoinKind::Full] {
            assert!(kind.mirrored().is_some(), "{kind:?} lost its mirror");
        }
    }
}
