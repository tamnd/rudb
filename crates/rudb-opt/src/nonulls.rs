//! Answering the null questions a store has already answered, on a column that has no nulls.
//!
//! A store that counted the nulls of a column and counted zero has settled every question about the
//! nulls of that column. So `x IS NULL` over it is `FALSE`, `x IS NOT NULL` is `TRUE`, and neither
//! one has to be asked of a row. `count(x)` over it is `count(*)`, which is a counter rather than a
//! walk of the mask. That is the rewrite section 5.10 of `spec/stats/05-every-query.md` asks for, and
//! what it buys is a branch taken out of the inner loop rather than a better guess about how often
//! the branch is taken.
//!
//! The kernels underneath were already validity free wherever the vector they are handed says so
//! itself. A native page carries a flag saying it holds no nulls, Parquet's presence levels collapse
//! to the same thing, and a mask that turns out to be all ones is normalized as it is built. What
//! none of that can do is settle a question about the whole column, and a predicate is about the
//! whole column: a chunk with no nulls in it says nothing about the chunk after it, so the kernel
//! still has to be able to handle one. The chunk decides the kernel and the store decides the plan.
//!
//! # Exact and nothing else
//!
//! The count is read through `estimate::NO_NULLS`, which is [`Use::Enable`]. An estimated zero is a
//! column nobody found a null in, which is a different claim from a column that has none, and a
//! predicate rewritten off the weaker claim drops rows. So a count the store arrived at by sampling
//! does not get through here, however likely it is to be right.
//!
//! The whole pass is behind [`Rule::ValidityFree`] as well, so the ablation in
//! `spec/stats/09-measurement.md` section 9.3 can turn it off and compare the answers.
//!
//! # Where this sits in the sequence
//!
//! After filter pushdown, because a predicate that has been pushed down sits close to the scan it
//! reads from, and what this has to do is walk from the operator holding the expression down to that
//! scan.
//!
//! After join elimination too, which is the ordering the walk forces. That pass turns a left join a
//! certificate covers into an inner join, and an inner join is one the walk goes through where a left
//! join is one it stops at. Running before it would mean refusing every column under such a join and
//! then watching the padding disappear a moment later, which is a rewrite the next statement gets and
//! this one does not.
//!
//! Before the empty result pullup, because a predicate that settles to `FALSE` is a scan nobody has to
//! run and that pass is what takes it out.
//!
//! What this pass leaves behind is work for two passes that do not run after it. A `count(x)` that
//! becomes a `count(*)` stops reading a column, and can be the last reader of a whole joined table.
//! So [`crate::columns::prune`] and [`crate::eliminate::sweep`] are called from `run` once anything
//! was rewritten, rather than left for the next run of the sequence to notice.
//!
//! # What is not rewritten
//!
//! Anything read through a join that pads. A left join writes nulls into every column of the side it
//! found no match on, so a column the file counted no nulls in has nulls in the rows above the join,
//! and `LEFT JOIN ... WHERE parent.x IS NULL` is how an anti join is written. The walk down to the
//! scan stops at an outer join for that reason, which means the question is asked of the file only
//! where the file is still the authority on the answer.
//!
//! A null test in a projection, which is one cheap kernel over a vector rather than a branch inside a
//! loop, and reaching it would mean walking every expression slot of every node for a rewrite that
//! does not pay for the walk. A join condition, for the same reason plus a worse one: an equality in
//! a join condition is what decides the join's algorithm, and a pass that edits conditions has to
//! leave that decision exactly where it found it.
//!
//! `count(DISTINCT x)`, which is a distinct count and not a row count. A column with no nulls has
//! nothing to say about how many of its values repeat.
//!
//! [`Use::Enable`]: rudb_common::stat::Use::Enable

use rudb_common::rules::Rule;
use rudb_common::{Result, Value};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass};
use crate::{columns, eliminate, estimate, fold, link, walk};

/// Takes the null branch out of a filter and out of a `count` where the store says there is none.
#[derive(Debug, Clone, Copy)]
pub struct NoNulls;

impl Pass for NoNulls {
    fn name(&self) -> &'static str {
        "no_nulls"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.allows(Rule::ValidityFree) && settle(plan) {
            // A `count` that stopped naming a column left that column read by nobody, and a parent
            // side of a join that nobody reads is a join that can go. Both of the passes that do
            // that work sit elsewhere in the sequence, one later and one earlier, so both are asked
            // for by hand here rather than waiting for the next run of the sequence to reach them.
            // Running the sequence twice has to be running it once, which is what the idempotence
            // assertion in `spec/09-optimizer.md` section 9.1 checks.
            columns::prune(plan);
            eliminate::sweep(plan, context);
        }
        Ok(())
    }
}

/// Answers every null question the store has answered, in the two places one costs work per row.
///
/// The answer is whether anything was rewritten, which is what tells the caller whether the plan
/// needs the columns nobody reads any more taken out of it.
fn settle(plan: &mut Plan) -> bool {
    let consumers = link::consumers(plan);
    let mut rewrote = false;
    for at in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        match *plan.node(at) {
            Node::Filter { input, predicate } => {
                let decided = decided(plan, input, predicate);
                if decided == predicate {
                    continue;
                }
                rewrote = true;
                // Folded again rather than left as written, because what this substitutes into is a
                // conjunction and `TRUE AND x` is not a predicate anybody should be evaluating. The
                // rules that collapse it ran before this pass did, so this asks for them by hand.
                let decided = fold::rewritten(plan, decided);
                if certain(plan, decided) {
                    eliminate::stand_in(plan, at, input, &consumers);
                    continue;
                }
                match plan.node_mut(at) {
                    Node::Filter { predicate, .. } => *predicate = decided,
                    _ => unreachable!("the node was a filter a moment ago"),
                }
            }
            Node::Aggregate { input, aggregates, .. } => {
                let Some(rewritten) = counted(plan, input, aggregates) else { continue };
                rewrote = true;
                match plan.node_mut(at) {
                    Node::Aggregate { aggregates, .. } => *aggregates = rewritten,
                    _ => unreachable!("the node was an aggregate a moment ago"),
                }
            }
            _ => {}
        }
    }
    rewrote
}

/// Whether this expression is the constant `TRUE`, which is a filter that filters nothing.
///
/// The other constant is not asked about here. A predicate that settled to `FALSE` is a scan nobody
/// has to run, and pulling an empty result up through the plan above it is a pass of its own that
/// runs after this one.
fn certain(plan: &Plan, expr: ExprRef) -> bool {
    matches!(*plan.expr(expr), Expr::Constant(value) if plan.value(value) == &Value::Boolean(true))
}

/// The expression with every null test over a column the store proved has no nulls replaced.
///
/// Bottom up, so a test buried under an `AND` and an `OR` is reached. The replacement keeps the type
/// the expression already had rather than asserting `BOOLEAN`, because the plan records one type per
/// expression and the comparison the binder built is the authority on its own.
fn decided(plan: &mut Plan, input: NodeRef, expr: ExprRef) -> ExprRef {
    let rebuilt = walk::rebuild(plan, expr, &mut |plan, child| decided(plan, input, child));
    let Expr::Compare { op, left, right } = *plan.expr(rebuilt) else { return rebuilt };
    // `IS NULL` binds to `IS NOT DISTINCT FROM NULL` and `IS NOT NULL` to `IS DISTINCT FROM NULL`,
    // which is what makes both of them a comparison here rather than a function.
    let answer = match op {
        CompareOp::NotDistinctFrom => false,
        CompareOp::DistinctFrom => true,
        _ => return rebuilt,
    };
    let Some(binding) = tested(plan, left, right) else { return rebuilt };
    if !estimate::never_null(plan, input, binding) {
        return rebuilt;
    }
    let ty = plan.expr_type(rebuilt).clone();
    let span = plan.expr_span(rebuilt);
    let value = plan.add_value(Value::Boolean(answer));
    plan.add_expr_at(Expr::Constant(value), ty, span)
}

/// The column a null test is about, where the two operands are a column and a null.
///
/// A column on either side, since `NULL IS NOT DISTINCT FROM x` is the same question written the
/// other way round and nothing obliges the binder to have normalized it.
fn tested(plan: &Plan, left: ExprRef, right: ExprRef) -> Option<ColumnBinding> {
    let null = |expr| matches!(*plan.expr(expr), Expr::Constant(v) if plan.value(v).is_null());
    let column = |expr| match *plan.expr(expr) {
        Expr::Column(binding) => Some(binding),
        _ => None,
    };
    column(left).filter(|_| null(right)).or_else(|| column(right).filter(|_| null(left)))
}

/// The aggregate list with every `count` of a column that has no nulls turned into `count(*)`.
///
/// `None` where nothing changed, so the caller does not append a copy of the list it already had.
///
/// A `FILTER` is carried across rather than refused. It decides which rows are counted, and that is
/// the same question whichever of the two counts is asking it.
fn counted(plan: &mut Plan, input: NodeRef, aggregates: Slice) -> Option<Slice> {
    let held = plan.expr_list(aggregates).to_vec();
    let mut rewritten = held.clone();
    for slot in &mut rewritten {
        let Expr::Aggregate { name, args, distinct: false, filter } = *plan.expr(*slot) else {
            continue;
        };
        if plan.string(name) != "count" {
            continue;
        }
        let [argument] = *plan.expr_list(args) else { continue };
        let Expr::Column(binding) = *plan.expr(argument) else { continue };
        if !estimate::never_null(plan, input, binding) {
            continue;
        }
        let name = plan.intern("count_star");
        let ty = plan.expr_type(*slot).clone();
        let span = plan.expr_span(*slot);
        let aggregate = Expr::Aggregate { name, args: Slice::EMPTY, distinct: false, filter };
        *slot = plan.add_expr_at(aggregate, ty, span);
    }
    (rewritten != held).then(|| plan.add_expr_list(&rewritten))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::Stat;
    use rudb_common::bounds::{Bound, End, Spread, Test, Zones};
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use super::settle;

    /// A store of one column called `d` that says this much about how many nulls the column holds.
    #[derive(Debug)]
    struct Stub(Stat<u64>);

    impl Stub {
        fn new(nulls: Stat<u64>) -> Arc<Self> {
            Arc::new(Self(nulls))
        }
    }

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            (name == "d").then_some(0)
        }

        fn surviving(&self, _tests: &[Test]) -> Option<u64> {
            None
        }

        fn spread(&self, _tests: &[Test]) -> Option<Spread> {
            None
        }

        fn extreme(&self, _column: usize, _end: End) -> Stat<Bound> {
            Stat::Unknown
        }

        fn nulls(&self, _column: usize) -> Stat<u64> {
            self.0
        }
    }

    /// A store that counted the column's nulls and counted none.
    fn empty() -> Arc<Stub> {
        Stub::new(Stat::exact(0, Provenance::NullCount))
    }

    /// What the plan a text prints looks like once the pass has run over a scan with that store.
    fn settled(text: &str, zones: &Arc<Stub>) -> String {
        settled_at(text, 0, zones)
    }

    /// The same, where the scan holding the store is not the first one in the text.
    fn settled_at(text: &str, index: u32, zones: &Arc<Stub>) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_zones(index, Arc::clone(zones) as Arc<dyn Zones>);
        settle(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// `WHERE d IS NOT NULL`, which the plan parser wants with the null's type written out and which
    /// prints back the same way.
    const NOT_NULL: &str = "Filter (#0.0::INTEGER IS DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  \
                            Get memory.main.t AS t #0 [d::INTEGER]\n";

    #[test]
    fn a_filter_that_only_asks_whether_a_column_with_no_nulls_is_null_goes_away_entirely() {
        assert_eq!(settled(NOT_NULL, &empty()), "Get memory.main.t AS t #0 [d::INTEGER]\n");
    }

    #[test]
    fn the_other_half_of_the_question_answers_false_and_the_filter_keeps_every_row_out() {
        let text = "Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(
            settled(text, &empty()),
            "Filter FALSE::BOOLEAN\n  Get memory.main.t AS t #0 [d::INTEGER]\n"
        );
    }

    #[test]
    fn a_settled_half_of_a_conjunction_leaves_the_half_that_is_still_a_question() {
        let text = "Filter ((#0.0::INTEGER IS DISTINCT FROM NULL::INTEGER)::BOOLEAN AND \
                    (#0.0::INTEGER > 50::INTEGER)::BOOLEAN)::BOOLEAN\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(
            settled(text, &empty()),
            "Filter (#0.0::INTEGER > 50::INTEGER)::BOOLEAN\n  \
             Get memory.main.t AS t #0 [d::INTEGER]\n"
        );
    }

    #[test]
    fn a_count_of_a_column_with_no_nulls_is_a_count_of_the_rows() {
        let text = "Aggregate #1 groups=[] aggregates=[count(#0.0::INTEGER)::BIGINT]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(
            settled(text, &empty()),
            "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT]\n  \
             Get memory.main.t AS t #0 [d::INTEGER]\n"
        );
    }

    #[test]
    fn a_distinct_count_is_not_a_row_count_however_few_nulls_the_column_holds() {
        let text = "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT]\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        assert_eq!(settled(text, &empty()), text);
    }

    #[test]
    fn a_null_count_the_store_estimated_is_not_a_proof_and_nothing_is_rewritten() {
        let guessed = Stub::new(Stat::estimated(0, Provenance::NullCount));
        assert_eq!(settled(NOT_NULL, &guessed), NOT_NULL);
    }

    #[test]
    fn a_column_the_store_counted_nulls_in_keeps_its_question() {
        let some = Stub::new(Stat::exact(7, Provenance::NullCount));
        assert_eq!(settled(NOT_NULL, &some), NOT_NULL);
    }

    #[test]
    fn a_table_that_says_nothing_about_its_nulls_keeps_its_question_too() {
        assert_eq!(settled(NOT_NULL, &Stub::new(Stat::Unknown)), NOT_NULL);
    }

    /// The anti join idiom: a left join, and a question about the padded side above it.
    ///
    /// `{}` is the join kind, because the whole point of the pair of tests below is that the kind is
    /// what decides the answer and everything else about the two plans is the same.
    fn padded(kind: &str) -> String {
        format!(
            "Filter (#1.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN\n  \
             Join {kind} on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n    \
             Get memory.main.t AS t #0 [d::INTEGER]\n    \
             Get memory.main.u AS u #1 [d::INTEGER]\n"
        )
    }

    #[test]
    fn a_question_about_the_side_a_left_join_pads_is_not_the_file_s_question() {
        // `LEFT JOIN ... WHERE u.d IS NULL` asks which rows of `t` have no match in `u`, and the
        // nulls it is asking about are the join's rather than the column's. The file counted none in
        // the column and that is not an answer to this, so the filter stays exactly as it was.
        let text = padded("LEFT");
        assert_eq!(settled_at(&text, 1, &empty()), text);
    }

    #[test]
    fn the_same_question_over_an_inner_join_is_the_file_s_question_after_all() {
        // Nothing is padded here, so every value the filter sees is a value the file wrote and the
        // count is about it. This is the control for the test above: one word of the plan differs.
        let settled = settled_at(&padded("INNER"), 1, &empty());
        assert!(settled.contains("Filter FALSE::BOOLEAN"), "{settled}");
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let text = "Filter ((#0.0::INTEGER IS DISTINCT FROM NULL::INTEGER)::BOOLEAN AND \
                    (#0.0::INTEGER > 50::INTEGER)::BOOLEAN)::BOOLEAN\n  \
                    Get memory.main.t AS t #0 [d::INTEGER]\n";
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        plan.set_zones(0, empty() as Arc<dyn Zones>);
        settle(&mut plan);
        let once = plan.to_string();
        settle(&mut plan);
        assert_eq!(plan.to_string(), once, "the pass did not settle");
    }
}
