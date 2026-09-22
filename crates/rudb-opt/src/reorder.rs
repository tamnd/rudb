//! Which conjunct of a filter runs first.
//!
//! A filter of several conjuncts runs them one at a time and stops on a row as soon as one of them
//! rejects it, so the order they are written in is most of what the filter costs. A predicate of
//! four conjuncts where one throws away almost every row costs a quarter as much with that one in
//! front as it does with that one at the back. This puts them in the order the catalog's numbers
//! say is cheapest.
//!
//! `spec/stats/09-measurement.md` section 9.4 is the reason there is a threshold rather than just a
//! sort: "filter reordering on a query with two cheap conjuncts costs the reordering and buys
//! nothing, and the response is a threshold, not a shrug". The cost being talked about is not the
//! sort here, which happens once per plan. It is that a plan whose predicate is written in a
//! different order than the user wrote it is a plan that is harder to read in `EXPLAIN` and a
//! rewrite that has to be right, and buying nothing for that is not worth it.
//!
//! # This is the second of two orderings and not the only one
//!
//! `rudb-exec`'s `ordering` module asks the same question again while the query runs, from what the
//! last sixteen chunks actually did, and it will beat this one wherever the two disagree, because it
//! is measuring and this is estimating. What it cannot do is be right on the first chunk. It starts
//! from the order the plan gave it and needs a window of chunks before it has anything of its own to
//! say, so the plan's order is what runs over the front of every scan and the whole of any scan
//! shorter than the window. A table of ten thousand rows is ten chunks and never leaves the plan's
//! order at all.
//!
//! The two use the same rank and the same cost model on purpose. If this pass put a conjunct in
//! front on the strength of an estimate and the runtime moved it back on the strength of a
//! measurement, that is the system working. If they disagreed because one divides by cost and the
//! other does not, that is two answers to one question and one of them is wrong.
//!
//! # What is not reordered
//!
//! A predicate that is not a top level `AND`. An `OR` is one condition however many branches it has,
//! and the runtime ordering handles the inside of one.
//!
//! A predicate holding a volatile call. Reordering changes how many rows a conjunct is asked about,
//! and a conjunct that is a different answer each time it is asked would be a different filter.
//!
//! A predicate where every conjunct's selectivity came from the constant, which is every conjunct
//! ranking the same on the half of the rank that matters. Sorting on cost alone would put the cheap
//! conjunct in front, which is right on average and is not what the constant knows.

use rudb_common::rules::Rule;
use rudb_common::{LogicalType, PhysicalType, Result};
use rudb_plan::{ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::estimate::{self, Facts};
use crate::pass::{Context, Pass, top_down};
use crate::walk;

/// How much cheaper the new order has to be before the predicate is rewritten.
///
/// A fifth. Below that the estimate is not telling the two orders apart: the selectivities going
/// into it are the catalog's, the costs are a ranking rather than a measurement, and a modelled
/// saving of a few percent is inside the error of both. Above it, one conjunct is doing visibly more
/// work than another and the order is worth changing.
const WORTH_REWRITING: f64 = 0.2;

/// What a conjunct is charged when the cost model says it is free.
///
/// A bare column reference is read in place and costs nothing to evaluate, which is true and is not
/// something to divide by. The floor is a quarter, which is what one constant vector costs, so a
/// free conjunct still ranks above everything with real work in it without ranking infinitely above
/// it.
const CHEAPEST: f64 = 0.25;

/// Puts the conjuncts of each filter in the order the statistics say is cheapest.
#[derive(Debug)]
pub struct FilterOrder;

impl Pass for FilterOrder {
    fn name(&self) -> &'static str {
        "reorder_filter"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if !context.allows(Rule::FilterOrder) {
            return Ok(());
        }
        for node in top_down(plan) {
            let Node::Filter { input, predicate } = *plan.node(node) else { continue };
            if let Some(ordered) = ordered(plan, input, predicate, context.facts()) {
                let rewritten = plan.add_expr(
                    Expr::Conjunction { op: ConjunctionOp::And, children: ordered },
                    LogicalType::Boolean,
                );
                if let Node::Filter { predicate, .. } = plan.node_mut(node) {
                    *predicate = rewritten;
                }
            }
        }
        Ok(())
    }
}

/// The conjuncts of `predicate` in their new order, or `None` to leave the predicate alone.
///
/// The list is built rather than sorted in place so that the decision to rewrite is made before
/// anything is written. A pass that rewrote first and checked afterwards would have to be able to
/// put a predicate back, and the plan has no way of forgetting an expression it has already added.
fn ordered(plan: &mut Plan, input: NodeRef, predicate: ExprRef, stats: &Facts) -> Option<Slice> {
    let Expr::Conjunction { op: ConjunctionOp::And, children } = *plan.expr(predicate) else {
        return None;
    };
    let parts = plan.expr_list(children).to_vec();
    if parts.len() < 2 || parts.iter().any(|&part| walk::volatile(plan, part)) {
        return None;
    }
    let mut measured = false;
    let mut scored = Vec::with_capacity(parts.len());
    for &part in &parts {
        let (kept, from) = estimate::kept_by(plan, input, part, stats);
        measured |= from != estimate::FROM_A_CONSTANT;
        scored.push(Conjunct { part, kept, cost: cost(plan, part).max(CHEAPEST) });
    }
    if !measured {
        return None;
    }
    // A stable sort, so two conjuncts the numbers cannot tell apart stay in the order the user
    // wrote them. That is what makes this idempotent: a predicate this pass has already ordered is
    // a predicate whose ranks are already descending, so the sort moves nothing and the two costs
    // below come out equal.
    let mut run = scored.clone();
    run.sort_by(|left, right| right.rank().total_cmp(&left.rank()));
    if modelled(&run) > modelled(&scored) * (1.0 - WORTH_REWRITING) {
        return None;
    }
    let reordered: Vec<ExprRef> = run.iter().map(|conjunct| conjunct.part).collect();
    Some(plan.add_expr_list(&reordered))
}

/// One conjunct, with what it keeps and what it costs.
#[derive(Clone, Copy)]
struct Conjunct {
    /// The condition itself.
    part: ExprRef,
    /// The fraction of rows it is expected to keep.
    kept: f64,
    /// What evaluating it costs, against one fixed width comparison as the unit.
    cost: f64,
}

impl Conjunct {
    /// What running this conjunct early is worth, which is the work it takes off the conjuncts
    /// behind it for what it costs to run.
    ///
    /// The fraction it rejects over what it costs, which is the same rank `rudb-exec`'s `ordering`
    /// module computes from measurements. Dividing by the cost is what keeps a `LIKE` that rejects
    /// everything from going in front of an integer comparison that rejects nearly everything for a
    /// twentieth of the work.
    fn rank(&self) -> f64 {
        (1.0 - self.kept) / self.cost
    }
}

/// What a whole conjunction costs to run over one row, in this order.
///
/// The first conjunct is asked about every row, the second about the rows the first kept, and so on
/// down. That product is the whole of what an order is worth and it is why the rank above is the
/// right one to sort by: the order that minimises this sum is the order the ranks come out
/// descending in.
fn modelled(conjuncts: &[Conjunct]) -> f64 {
    let mut total = 0.0;
    let mut reaching = 1.0;
    for conjunct in conjuncts {
        total += reaching * conjunct.cost;
        reaching *= conjunct.kept;
    }
    total
}

/// Roughly what one expression costs to evaluate, against a comparison of two fixed width columns
/// as the unit.
///
/// The same weights `rudb-exec`'s `Prepared::weight` charges, because the two are ordering the same
/// conjuncts and a disagreement between them would be the plan and the runtime pulling in opposite
/// directions for no reason anybody could read off either one. A ranking rather than a prediction:
/// nothing reads the number itself, only which of two of them is larger, and the differences that
/// decide an order are the big ones.
fn cost(plan: &Plan, expr: ExprRef) -> f64 {
    match *plan.expr(expr) {
        // Read straight out of the chunk at the point it is wanted, so there is nothing to run.
        Expr::Column(_) => 0.0,
        // One vector built per chunk, however many rows the chunk has.
        Expr::Constant(_) => 0.25,
        // The operands carry the cost of a connective, and they are what is charged for.
        Expr::Conjunction { children, .. } => summed(plan, children),
        Expr::Cast { input, .. } => 2.0 * touching(plan.expr_type(input)) + cost(plan, input),
        Expr::Compare { left, right, .. } => {
            touching(plan.expr_type(left)) + cost(plan, left) + cost(plan, right)
        }
        Expr::Function { args, .. } => {
            let widest = plan
                .expr_list(args)
                .iter()
                .map(|&argument| touching(plan.expr_type(argument)))
                .fold(1.0, f64::max);
            4.0 * widest + summed(plan, args)
        }
        // A branch per arm, each of which is an expression of its own that this does not look
        // inside. Charging for the arms alone understates it and says the right thing about the
        // order, which is that a `CASE` is not what you want in front.
        Expr::Case { arms, .. } => 4.0 * plan.arm_list(arms).len() as f64,
        // An aggregate or a window inside a filter is a shape the binder does not build, and
        // costing one would be inventing a number for something that cannot get here.
        Expr::Aggregate { .. } | Expr::Window { .. } => 0.0,
    }
}

/// What a run of expressions costs together.
fn summed(plan: &Plan, slice: Slice) -> f64 {
    plan.expr_list(slice).iter().map(|&expr| cost(plan, expr)).sum()
}

/// What touching one value of a type costs, relative to a fixed width one.
fn touching(ty: &LogicalType) -> f64 {
    match ty.physical() {
        PhysicalType::Varlen => 4.0,
        PhysicalType::List | PhysicalType::Array | PhysicalType::Struct => 8.0,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Provenance;
    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    use super::FilterOrder;
    use crate::estimate::Facts;
    use crate::pass::{Context, Pass};

    /// A scan of a table with a wide column and a narrow one, under a filter of two conjuncts.
    ///
    /// `n_nationkey` is counted at twenty five values and `n_comment` is a string nobody counted, so
    /// the equality on the key is the selective one and the function on the comment is the dear one.
    /// Every test here is about which of those two runs first.
    fn filtered(predicate: &str) -> Plan {
        let text = format!(
            "Filter {predicate}\n  \
             Get memory.main.nation AS nation #0 [n_nationkey::BIGINT, n_comment::VARCHAR]\n"
        );
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    /// A context that has been told what the nation columns hold.
    fn counted() -> Context {
        let mut facts = Facts::new();
        facts.record("memory", "main", "nation", 25);
        facts.record_distinct("memory", "main", "nation", "n_nationkey", 25, Provenance::Sketch);
        let mut context = Context::new();
        context.measure(std::sync::Arc::new(facts));
        context
    }

    fn rewritten(plan: &mut Plan, context: &Context) -> String {
        FilterOrder.run(plan, context).expect("the pass does not fail");
        plan.to_string()
    }

    /// Which conjunct the filter runs first, by where it sits in the printed predicate.
    fn leading(text: &str) -> String {
        let line = text.lines().find(|line| line.contains("Filter")).expect("a filter is printed");
        line.split(" AND ").next().expect("a conjunction prints its parts").to_owned()
    }

    #[test]
    fn the_conjunct_that_throws_most_away_per_unit_of_work_goes_in_front() {
        let mut plan = filtered(
            "((upper(#0.1::VARCHAR)::VARCHAR = 'X'::VARCHAR)::BOOLEAN \
             AND (#0.0::BIGINT = 3::BIGINT)::BOOLEAN)::BOOLEAN",
        );
        let text = rewritten(&mut plan, &counted());
        assert!(
            leading(&text).contains("#0.0"),
            "the counted equality is both cheaper and more selective: {text}"
        );
    }

    #[test]
    fn a_predicate_already_in_the_best_order_is_left_alone() {
        let predicate = "((#0.0::BIGINT = 3::BIGINT)::BOOLEAN \
             AND (upper(#0.1::VARCHAR)::VARCHAR = 'X'::VARCHAR)::BOOLEAN)::BOOLEAN";
        let mut plan = filtered(predicate);
        let before = plan.to_string();
        let text = rewritten(&mut plan, &counted());
        assert_eq!(text, before, "there was nothing to move");
    }

    #[test]
    fn two_conjuncts_nobody_counted_are_left_in_the_order_they_were_written() {
        // Both selectivities come from the constant, so the only thing separating the two is cost,
        // and sorting on cost alone would be acting on a number the estimate does not have. This is
        // the shape section 9.4 calls reordering that costs the reordering and buys nothing.
        let mut plan = filtered(
            "((upper(#0.1::VARCHAR)::VARCHAR = 'X'::VARCHAR)::BOOLEAN \
             AND (#0.1::VARCHAR = 'y'::VARCHAR)::BOOLEAN)::BOOLEAN",
        );
        let before = plan.to_string();
        let text = rewritten(&mut plan, &Context::new());
        assert_eq!(text, before, "no conjunct here was measured");
    }

    #[test]
    fn a_saving_below_the_threshold_is_not_worth_rewriting_the_predicate_for() {
        // Two equalities on the same counted column cost the same and keep the same fraction, so
        // whichever order they are in the model says the same number and the threshold refuses.
        let mut plan = filtered(
            "((#0.0::BIGINT = 3::BIGINT)::BOOLEAN \
             AND (#0.0::BIGINT = 4::BIGINT)::BOOLEAN)::BOOLEAN",
        );
        let before = plan.to_string();
        let text = rewritten(&mut plan, &counted());
        assert_eq!(text, before, "the two orders cost the same");
    }

    #[test]
    fn the_rule_turns_the_whole_pass_off() {
        let mut plan = filtered(
            "((upper(#0.1::VARCHAR)::VARCHAR = 'X'::VARCHAR)::BOOLEAN \
             AND (#0.0::BIGINT = 3::BIGINT)::BOOLEAN)::BOOLEAN",
        );
        let before = plan.to_string();
        let mut rules = Rules::new();
        rules.set(Rule::FilterOrder, false);
        let mut context = counted();
        context.govern(rules);
        let text = rewritten(&mut plan, &context);
        assert_eq!(text, before, "the setting is the whole of the switch");
    }

    #[test]
    fn running_the_pass_twice_gives_the_same_plan() {
        let mut plan = filtered(
            "((upper(#0.1::VARCHAR)::VARCHAR = 'X'::VARCHAR)::BOOLEAN \
             AND (#0.0::BIGINT = 3::BIGINT)::BOOLEAN)::BOOLEAN",
        );
        let context = counted();
        let once = rewritten(&mut plan, &context);
        let twice = rewritten(&mut plan, &context);
        assert_eq!(once, twice, "a predicate this pass ordered is already in order");
    }
}
