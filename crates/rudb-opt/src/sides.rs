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
//! It depends on which operator is going to run, and there are two, which want opposite things.
//!
//! "Build side" is hash join vocabulary, where the rule is to build from the smaller input because
//! the hash table is what has to fit in memory and building it is paid for once per gathered row
//! where reading it is paid for once per driving row. That operator now exists. It arrived in 0.3.41
//! as the lookup in `crates/rudb-exec/src/join.rs` and became a stream in 0.3.42, and it runs
//! whenever every condition the join holds is an equality between a column of one side and a column
//! of the other.
//!
//! When any condition is not, the same file's nested loop runs instead, and it has the opposite
//! preference. It holds the gathered side as chunks and, for each row of the other side, walks every
//! one of those chunks and evaluates the conditions over it. So the number of calls into the
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
//! The ratio grows with the ratio between the sides, which is what the count above predicts, so this
//! is the shape of the operator rather than a constant factor somewhere.
//!
//! So this pass asks which operator the join will get before it asks which side is bigger, and
//! `crate::filter::lookup` is that question, asked of the finished condition list rather than of one
//! being rewritten. Between the lookup landing and this, every hash join in rudb was building its
//! table out of the larger of its two inputs, because this pass was still written for the operator
//! that used to be the only one.
//!
//! The flag on the node says *which side is gathered* rather than *which side is smaller*, and that
//! is what lets the two operators share it: the first is a fact about the plan they agree on and the
//! second is a policy they disagree about.
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
//!
//! # An outer join has a side it cannot gather
//!
//! An `OUTER` join keeps the rows of one side whether or not anything matched them, and which of
//! them matched nothing is not known until the last row of the other side has been through. So the
//! lookup cannot answer it while gathering that side: it decides about a driving row from that
//! row's own matches, which is what lets it answer as it goes, and a row of the gathered side that
//! nothing has matched yet may still be matched by a driving row that has not arrived.
//! `crates/rudb-exec/src/join.rs::streamed` is that list and `RIGHT` and `FULL` are not on it.
//!
//! Gathering the kept side of a `LEFT` join is the same thing as running a `RIGHT` join, because
//! that is what [`rudb_plan::JoinKind::mirrored`] says swapping the inputs means. So the size rule
//! on its own can take a join the lookup would have answered and hand it to the nested loop, which
//! builds a boxed row per pair, and on `customer LEFT JOIN orders` at SF1 it did: the estimate has
//! the kept side smaller, the pass gathered it, and counting the pairs took 0.392 s against 0.079 s
//! for the same join gathering the other side. TPC-H q13 is that join and it went 0.854 s to 0.460.
//!
//! So a kind that keeps a side does not get a choice when the join is a lookup. The side to gather
//! is the other one, whichever is bigger, because the alternative is not a smaller hash table, it
//! is no hash table. A `FULL` join keeps both sides and no lookup answers it either way, so it is
//! left to the size rule with the rest of the nested loops.

use rudb_plan::{BuildSide, JoinKind, Node, Plan};

use rudb_common::Result;

use crate::estimate::{self, Statistics};
use crate::filter;
use crate::pass::{Context, Pass, top_down};
use crate::tables::{Tables, produced};

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

/// Which side the operator would rather have gathered, given what each side is guessed to hold.
///
/// It depends on which operator, and the two want opposite things. A hash table is built out of the
/// gathered side and then read once per driving row, so the smaller side is the one to gather: it is
/// the side that has to fit in memory, and building it is the part that is paid for per row rather
/// than per lookup. A nested loop walks the gathered side a chunk at a time for each driving row, so
/// it wants the larger side gathered, which is the reasoning in the module documentation and the
/// measurements there.
///
/// `None` when the two are equal, which is not a preference.
#[must_use]
fn prefers(left: u64, right: u64, lookup: bool) -> Option<BuildSide> {
    let (bigger, smaller) = if lookup {
        (BuildSide::Right, BuildSide::Left)
    } else {
        (BuildSide::Left, BuildSide::Right)
    };
    match left.cmp(&right) {
        std::cmp::Ordering::Greater => Some(bigger),
        std::cmp::Ordering::Less => Some(smaller),
        std::cmp::Ordering::Equal => None,
    }
}

/// The side a lookup has no choice about, for a kind that keeps one side's rows whatever matches.
///
/// The kept side cannot be the gathered one, because a gathered row that nothing has matched yet
/// may still be matched, so the operator cannot say anything about it until the driving side is
/// finished, and the lookup answers a driving row as it arrives. Gathering it instead hands the
/// join to the nested loop, which is the argument in the module documentation and the measurement
/// there. Size does not come into it: the choice is between a hash table on the other side and no
/// hash table at all.
///
/// `None` for a nested loop, which runs the same way whichever side it holds, and for the kinds
/// that keep neither side or both.
#[must_use]
fn forced(kind: JoinKind, lookup: bool) -> Option<BuildSide> {
    if !lookup {
        return None;
    }
    match kind {
        JoinKind::Left => Some(BuildSide::Right),
        JoinKind::Right => Some(BuildSide::Left),
        _ => None,
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
    let mut tables = Tables::new();
    for node in top_down(plan) {
        let Node::Join { left, right, kind, conditions, .. } = *plan.node(node) else {
            continue;
        };
        if kind.mirrored().is_none() {
            continue;
        }
        let below = (produced(plan, left), produced(plan, right));
        let held: Vec<_> = plan.expr_list(conditions).to_vec();
        let lookup = filter::lookup(plan, &mut tables, &held, &below);
        let wanted = match forced(kind, lookup) {
            Some(side) => side,
            None => {
                let (Some(left), Some(right)) =
                    (estimate::rows(plan, left, stats), estimate::rows(plan, right, stats))
                else {
                    continue;
                };
                let Some(side) = prefers(left, right, lookup) else {
                    continue;
                };
                side
            }
        };
        if let Node::Join { build, .. } = plan.node_mut(node) {
            *build = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{BuildSide, JoinKind, Node, Plan};

    use super::{BuildSideProbeSide, forced, prefers};
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

    /// The same as `chosen`, with an equality across the sides so that the join takes the lookup.
    fn keyed(kind: &str, left: u64, right: u64) -> BuildSide {
        let text = format!(
            "Join {kind} on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT]\n"
        );
        side(&text, left, right)
    }

    /// Runs the pass over a plan written out in full and reports the side it wrote on the root.
    fn side(text: &str, left: u64, right: u64) -> BuildSide {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let mut statistics = Statistics::new();
        statistics.record("memory", "main", "l", left);
        statistics.record("memory", "main", "r", right);
        let mut context = Context::new();
        context.measure(statistics);
        BuildSideProbeSide.run(&mut plan, &context).expect("the pass does not fail");
        let Node::Join { build, .. } = *plan.node(plan.root()) else {
            panic!("the root stopped being a join");
        };
        build
    }

    #[test]
    fn the_larger_side_is_the_one_gathered_because_the_nested_loop_walks_it_a_chunk_at_a_time() {
        assert_eq!(prefers(100_000, 4, false), Some(BuildSide::Left));
        assert_eq!(prefers(4, 100_000, false), Some(BuildSide::Right));
    }

    #[test]
    fn the_smaller_side_is_the_one_gathered_when_a_hash_table_is_going_to_be_built_out_of_it() {
        assert_eq!(prefers(100_000, 4, true), Some(BuildSide::Right));
        assert_eq!(prefers(4, 100_000, true), Some(BuildSide::Left));
    }

    #[test]
    fn two_sides_of_the_same_size_are_not_a_preference() {
        assert_eq!(prefers(500, 500, false), None);
        assert_eq!(prefers(500, 500, true), None);
    }

    #[test]
    fn a_join_with_an_equality_gathers_the_small_side_and_the_same_join_without_one_does_not() {
        // The same two tables and the same estimates, differing only in whether the condition is one
        // the operator answers with a table. This is the whole of the change: a join of 400,000 rows
        // against 4 on a key used to gather the 400,000.
        assert_eq!(keyed("INNER", 400_000, 4), BuildSide::Right);
        assert_eq!(chosen("INNER", 400_000, 4), BuildSide::Left);
    }

    #[test]
    fn a_condition_no_lookup_answers_still_gathers_the_larger_side() {
        // One condition that is not a cross side equality is enough to put the nested loop back, and
        // the preference goes back with it.
        let text = "Join INNER on=[(#0.0::BIGINT < #1.0::BIGINT)::BOOLEAN]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT]\n";
        assert_eq!(side(text, 400_000, 4), BuildSide::Left);
    }

    #[test]
    fn a_big_left_and_a_small_right_gathers_the_left() {
        assert_eq!(chosen("INNER", 400_000, 4), BuildSide::Left);
    }

    #[test]
    fn a_small_left_and_a_big_right_keeps_the_side_the_binder_emitted() {
        assert_eq!(chosen("INNER", 4, 400_000), BuildSide::Right);
    }

    /// The mirror of a `LEFT` join is a `RIGHT` join, which the nested loop knows how to run, so a
    /// join no lookup answers is allowed to swap like an inner one.
    #[test]
    fn an_outer_join_whose_kind_has_a_mirror_still_gets_the_larger_side() {
        assert_eq!(chosen("LEFT", 400_000, 4), BuildSide::Left);
        assert_eq!(chosen("FULL", 400_000, 4), BuildSide::Left);
    }

    #[test]
    fn a_left_join_on_a_key_gathers_the_other_side_however_small_the_side_it_keeps_is() {
        // Four rows against four hundred thousand, and the four are the side the join keeps. The
        // size rule wants them gathered and cannot have them: gathering them is running a `RIGHT`
        // join, no lookup answers one, and the join would go to the nested loop instead.
        assert_eq!(keyed("LEFT", 4, 400_000), BuildSide::Right);
        assert_eq!(keyed("LEFT", 400_000, 4), BuildSide::Right);
    }

    #[test]
    fn a_right_join_on_a_key_is_the_same_rule_the_other_way_round() {
        assert_eq!(keyed("RIGHT", 400_000, 4), BuildSide::Left);
        assert_eq!(keyed("RIGHT", 4, 400_000), BuildSide::Left);
    }

    /// A `FULL` join keeps both sides, so no lookup answers it whichever side is gathered and there
    /// is nothing to protect. It goes back to the size rule with the other nested loops.
    #[test]
    fn a_full_join_on_a_key_still_gathers_the_smaller_side() {
        assert_eq!(keyed("FULL", 4, 400_000), BuildSide::Left);
        assert_eq!(keyed("FULL", 400_000, 4), BuildSide::Right);
    }

    #[test]
    fn the_side_an_outer_join_cannot_gather_is_the_side_it_keeps() {
        assert_eq!(forced(JoinKind::Left, true), Some(BuildSide::Right));
        assert_eq!(forced(JoinKind::Right, true), Some(BuildSide::Left));
        assert_eq!(forced(JoinKind::Full, true), None);
        assert_eq!(forced(JoinKind::Inner, true), None);
        // A nested loop runs the same way whichever side it holds, so there is nothing forced.
        assert_eq!(forced(JoinKind::Left, false), None);
        assert_eq!(forced(JoinKind::Right, false), None);
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
