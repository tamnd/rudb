//! Which input of each join the executor gathers whole.
//!
//! A join is two inputs and a dependency edge between them. One side is run to the end and held,
//! and then the other side's rows arrive and are matched against what was held. [`BuildSide`] says
//! which side that is, the binder emits the same answer for every join because at binding time
//! there is nothing to choose with, and this pass is what chooses.
//!
//! The choice is made from [`estimate::rows`] and, for a hash join, from how wide a row of each side
//! is. No sample, no histogram, no reordering: this pass never moves a join and never changes what
//! any join produces. It writes one field, and a plan it has run over is a plan whose answers are
//! the answers it had before.
//!
//! # Rows times width
//!
//! A hash table holds every column its side carries, so what has to fit in memory is the rows times
//! the width of one of them and not the rows alone. The two can point different ways. TPC-H q9 joins
//! `orders` on its key to what the rest of the query built out of `lineitem`, which is 319,404 rows
//! carrying seven columns including the nation's name, against 1.5 million orders carrying two. By
//! rows the lineitem side is the one to hold, by bytes it is the orders, and holding the orders took
//! the query's peak resident set on SF1 from 307 MB to 238 MB with the same answer and a little less
//! cpu. DuckDB weighs its build side the same way, by rows times the width of the row its hash table
//! lays out.
//!
//! The width is the size of each column's type, and eight bytes for the hash every row is stored
//! beside. A string counts as its sixteen byte header, the same number DuckDB uses, which says
//! nothing about how long the strings are. The nested loop keeps comparing rows, because what it
//! pays for is walking the gathered side a chunk at a time and a chunk is rows.
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
//! and [`rudb_plan::JoinKind::mirrored`] is where the kinds that have one are listed. `SINGLE` and
//! `MARK` produce the left input's rows, or a column about them, so their left input is the subject
//! rather than a side, and `POSITIONAL` pairs the nth row with the nth row, which nothing about one
//! input alone preserves. Those keep whatever they were given.
//!
//! `SEMI` and `ANTI` have no mirror either and do get a choice, because the executor has a second
//! operator for them rather than a second kind. `crates/rudb-exec/src/join.rs::Marking` gathers the
//! subject side, marks a gathered row when some row of the other side matches it, and produces the
//! marked rows, or the unmarked ones, once the other side is finished. So the subject can be the
//! gathered side after all, and when it is the smaller of the two it should be: TPC-H q21's
//! `EXISTS` is a semi join over a subject of about seventy five thousand rows and a `lineitem` of
//! six million, and the operator that cannot turn around has to build its table out of the six
//! million. Only when a lookup answers the join, since that second operator is a hash join and
//! there is no nested loop written the other way round.
//!
//! A join where either estimate is `None`, which is a scan of a table nobody measured and anything
//! above one. Guessing between two sides when one of the two numbers is missing is how an optimizer
//! talks itself into the plan that is five times slower, and the side the binder emitted is at
//! least the side every plan had before this pass existed.
//!
//! A join whose two sides estimate the same. There is nothing to choose between them and choosing
//! anyway would make the flag depend on which comparison operator was written down.
//!
//! # An outer join used to have a side it could not gather
//!
//! An `OUTER` join keeps the rows of one side whether or not anything matched them, and which of
//! them matched nothing is not known until the last row of the other side has been through. The
//! lookup decides about a driving row from that row's own matches, which is what lets it answer as
//! it goes, so `crates/rudb-exec/src/join.rs::streamed` leaves `RIGHT` and `FULL` off its list and
//! the kept side used to be forced to stream. Gathering it instead meant the nested loop and its
//! boxed row per pair, which on `customer LEFT JOIN orders` at SF1 was 0.392 s against 0.079 for
//! the same join gathering the other side.
//!
//! What that argument leaves out is that only part of such a join's answer has to wait. The pairs
//! are known as each driving row arrives, exactly as they are for an inner join. What is not known
//! until the end is the padded row owed to a gathered row nothing matched, and there are at most
//! as many of those as the gathered side has rows.
//! `crates/rudb-exec/src/join.rs::Padding` produces the first half as a stream and hands the
//! second half over once the driving side is finished, so the kept side can be gathered after all.
//!
//! Which matters because forcing the kept side to stream forces the pipeline's degree to be
//! whatever the kept side is worth. TPC-H q13 keeps the 150,000 row `customer` and joins it to the
//! 1.5 million row `orders`, so the 1.5 million probes ran at the degree 150,000 rows buy, which
//! on a ten thread machine is two of them, while the machine's other eight sat idle. DuckDB
//! answers the same query as a right outer join for the same reason.
//!
//! So an outer join is back on the size rule with everything else. When no lookup answers it the
//! nested loop runs and wants the larger side, which is what the size rule already says.

use rudb_plan::{BuildSide, JoinKind, Node, NodeRef, Plan};

use rudb_common::Result;

use crate::estimate::{self, Facts};
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
        choose(plan, context.facts());
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

/// Writes the chosen side onto every join in `plan` that has one.
///
/// In place rather than by rebuilding, because this changes no node's shape and no node's children.
/// Rebuilding would give every join above a rewritten join a new reference for no reason, and a
/// pass that moves every node is a pass whose output is hard to read against its input.
///
/// Idempotent by construction: the answer is a function of the two estimates and the kind, none of
/// which this pass touches, so a second run writes what is already there.
fn choose(plan: &mut Plan, stats: &Facts) {
    let mut tables = Tables::new();
    for node in top_down(plan) {
        let Node::Join { left, right, kind, conditions, .. } = *plan.node(node) else {
            continue;
        };
        let sides = (left, right);
        // A semi or an anti join has no mirror and is still a choice, because the executor runs a
        // turned around one as a different operator rather than as a different kind. See the
        // module documentation.
        let turned = matches!(kind, JoinKind::Semi | JoinKind::Anti);
        if kind.mirrored().is_none() && !turned {
            continue;
        }
        let below = (produced(plan, left), produced(plan, right));
        let held: Vec<_> = plan.expr_list(conditions).to_vec();
        let lookup = filter::lookup(plan, &mut tables, &held, &below);
        // That operator is a hash join and there is no nested loop written the other way round, so
        // a semi join the lookup cannot answer keeps the side the binder gave it.
        if turned && !lookup {
            continue;
        }
        let (Some(left), Some(right)) =
            (estimate::rows(plan, left, stats), estimate::rows(plan, right, stats))
        else {
            continue;
        };
        // A hash table holds everything its side carries, so for the lookup the comparison is of
        // bytes rather than rows wherever both widths can be read. See the module documentation.
        let (left, right) = match (lookup, width(plan, sides.0), width(plan, sides.1)) {
            (true, Some(one), Some(other)) => {
                (left.saturating_mul(one + HASHED), right.saturating_mul(other + HASHED))
            }
            _ => (left, right),
        };
        let Some(wanted) = prefers(left, right, lookup) else {
            continue;
        };
        if let Node::Join { build, .. } = plan.node_mut(node) {
            *build = wanted;
        }
    }
}

/// What a hash table keeps beside each row it holds, which is the row's hash.
const HASHED: u64 = 8;

/// What one row of what `node` produces takes, by the size of each column's type.
///
/// Read off the plan rather than estimated, because a node's columns and their types are fixed by
/// the time this pass runs: the unused ones are already gone and nothing after this adds any. The
/// nodes that pass their input's row through unchanged answer what their input answers, and a join
/// answers both of its sides except where its kind keeps only the left one. `None` for anything
/// else, which leaves that join on the row counts.
fn width(plan: &Plan, node: NodeRef) -> Option<u64> {
    let fields = |columns| -> u64 {
        plan.field_list(columns).iter().map(|field| field.ty.physical().size() as u64).sum()
    };
    let exprs = |list| -> u64 {
        plan.expr_list(list).iter().map(|&expr| plan.expr_type(expr).physical().size() as u64).sum()
    };
    match *plan.node(node) {
        Node::Get { columns, .. }
        | Node::Values { columns, .. }
        | Node::TableFunction { columns, .. }
        | Node::CteScan { columns, .. } => Some(fields(columns)),
        Node::Project { exprs: list, .. } => Some(exprs(list)),
        Node::Aggregate { groups, aggregates, .. } => Some(exprs(groups) + exprs(aggregates)),
        Node::Filter { input, .. }
        | Node::Sort { input, .. }
        | Node::Limit { input, .. }
        | Node::LimitPercent { input, .. }
        | Node::TopN { input, .. }
        | Node::Distinct { input, .. } => width(plan, input),
        Node::Join { left, right, kind, .. }
        | Node::LinkJoin { child: left, parent: right, kind, .. } => match kind {
            JoinKind::Semi | JoinKind::Anti => width(plan, left),
            JoinKind::Mark => Some(width(plan, left)? + 1),
            _ => Some(width(plan, left)? + width(plan, right)?),
        },
        Node::CrossProduct { left, right } => Some(width(plan, left)? + width(plan, right)?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{BuildSide, JoinKind, Node, Plan};

    use super::{BuildSideProbeSide, prefers};
    use crate::estimate::Facts;
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
        let mut facts = Facts::new();
        facts.record("memory", "main", "l", left);
        facts.record("memory", "main", "r", right);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
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
        let mut facts = Facts::new();
        facts.record("memory", "main", "l", left);
        facts.record("memory", "main", "r", right);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
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
    fn a_hash_join_gathers_the_side_with_fewer_bytes_even_when_it_has_more_rows() {
        // A hundred thousand rows of one integer against forty thousand rows of five strings and a
        // key, so the right side has fewer rows and three times the bytes.
        let text = "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT, c::VARCHAR, d::VARCHAR, e::VARCHAR, f::VARCHAR, g::VARCHAR]\n";
        assert_eq!(side(text, 100_000, 40_000), BuildSide::Left);
        // The same join on row counts alone would have gathered the right, and still does where the
        // widths are the same.
        assert_eq!(keyed("INNER", 100_000, 40_000), BuildSide::Right);
    }

    #[test]
    fn the_nested_loop_still_compares_rows_whatever_the_widths() {
        let text = "Join INNER on=[(#0.0::BIGINT < #1.0::BIGINT)::BOOLEAN]\n  Get memory.main.l AS l #0 [a::BIGINT]\n  Get memory.main.r AS r #1 [b::BIGINT, c::VARCHAR, d::VARCHAR, e::VARCHAR, f::VARCHAR, g::VARCHAR]\n";
        assert_eq!(side(text, 100_000, 40_000), BuildSide::Left);
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

    /// The side a `LEFT` join keeps is gatherable now, so the size rule decides it like any other.
    /// Four rows against four hundred thousand is gathered whichever input the four are, and the
    /// executor runs the second of those as `crates/rudb-exec/src/join.rs::Padding`.
    #[test]
    fn a_left_join_on_a_key_gathers_whichever_side_is_smaller() {
        assert_eq!(keyed("LEFT", 4, 400_000), BuildSide::Left);
        assert_eq!(keyed("LEFT", 400_000, 4), BuildSide::Right);
    }

    #[test]
    fn a_right_join_on_a_key_is_the_same_rule_the_other_way_round() {
        assert_eq!(keyed("RIGHT", 400_000, 4), BuildSide::Right);
        assert_eq!(keyed("RIGHT", 4, 400_000), BuildSide::Left);
    }

    #[test]
    fn a_full_join_on_a_key_still_gathers_the_smaller_side() {
        assert_eq!(keyed("FULL", 4, 400_000), BuildSide::Left);
        assert_eq!(keyed("FULL", 400_000, 4), BuildSide::Right);
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
        let mut facts = Facts::new();
        // Only one of the two sides, which is the case the module documentation calls out: one
        // number is not enough to choose with and a default in place of the other one would be a
        // guess wearing a measurement's name.
        facts.record("memory", "main", "l", 400_000);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
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
