//! Which of several existence tests over the same rows runs first.
//!
//! A semi join and an anti join add no columns. They keep some of the rows their left side hands
//! them, which is what a filter does, so a stack of them over one side is a filter of several
//! conjuncts written as joins, and the order they run in is most of what they cost for the same
//! reason [`crate::reorder`] gives for conjuncts: the second one only sees what the first one left.
//! The binder stacks them in the order the query wrote its `EXISTS` clauses, which says nothing
//! about which one throws more away.
//!
//! TPC-H q21 is the case. The `EXISTS` keeps a line of an order when another line of the same order
//! has a different supplier, and almost every line of a multi line order has one, so it keeps 151,237
//! of the 156,739 lines it is asked about on SF1. The `NOT EXISTS` after it keeps a line when no
//! other line of the order was late, and it keeps 8,357. Each of them walks the other lines of the
//! order for every line it is given, so running the second one first leaves the first one a
//! twentieth of the walking.
//!
//! # The estimate
//!
//! The constant the estimator gives every semi join cannot order them, since it is the same for
//! both. What can is the shape both of these have: the key is the same stored column on both sides,
//! so the rows a line is tested against are the other rows of its own group, and how many of those
//! there are is the table's rows over the key's distinct values. A line with `n` of them, each passing
//! the other side's conditions with chance `s`, has a match with chance `1 - (1 - s)^n`, which is
//! what a semi join keeps and one minus what an anti join keeps. The share `s` is what the filters on
//! the other side keep, from the same numbers [`crate::reorder`] reads, times a guess for any other
//! condition between the two sides. An inequality between the two sides keeps nearly every pair and
//! counts as keeping all of them, and when it compares the key's own table column with itself it
//! also says the row is not its own match, so `n` is one less.
//!
//! On q21 at SF1 that is `s = 1` and three other lines for the `EXISTS`, which keeps everything, and
//! the constant fifth for `l_receiptdate > l_commitdate` for the `NOT EXISTS`, which keeps about
//! half. The real numbers are 96 percent and 5 percent, so the estimate is wrong about the second
//! and still puts it first, which is the only thing it is used for here.
//!
//! # What is not reordered
//!
//! A stack where any test is not over its own table's group, since that is the only shape the
//! estimate reads and one test guessed at the constant beside one read from the statistics would be
//! ordered on a number that knows nothing. A single test, which has nothing to go before. Anything
//! between two tests that is not itself one, so a filter or a projection in the middle of the stack
//! ends it.

use rudb_common::Result;
use rudb_common::rules::Rule;
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
};

use crate::eliminate::indices;
use crate::estimate::{self, CARDINALITY, DISTINCT, Facts, KEPT_BY_A_CONDITION};
use crate::pass::{Context, Pass};
use crate::walk;

/// Puts a stack of semi and anti joins over one side in the order that drops rows soonest.
///
/// Under [`Rule::FilterOrder`], because it is the same decision [`crate::reorder`] makes about the
/// conjuncts of a filter and `SET stats_filter_order = 'off'` is expected to turn both off.
#[derive(Debug)]
pub struct ExistsOrder;

impl Pass for ExistsOrder {
    fn name(&self) -> &'static str {
        "exists_order"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if !context.allows(Rule::FilterOrder) {
            return Ok(());
        }
        let mut changed = false;
        let root = plan.root();
        let rebuilt = walk::restack(plan, root, &mut changed, &mut |plan, at| {
            ordered(plan, at, context.facts())
        });
        if changed {
            plan.set_root(rebuilt);
        }
        Ok(())
    }
}

/// The stack of existence tests topped by `at`, rebuilt with the one that keeps least at the bottom.
///
/// `None` for a stack of one, for a stack where a test has no estimate, and for a stack already in
/// that order, which is what makes a second run leave the plan alone. Ties keep the order they had.
fn ordered(plan: &mut Plan, at: NodeRef, stats: &Facts) -> Option<NodeRef> {
    let mut stack = Vec::new();
    let mut base = at;
    while let Node::Join { left, kind: JoinKind::Semi | JoinKind::Anti, .. } = *plan.node(base) {
        stack.push(base);
        base = left;
    }
    if stack.len() < 2 {
        return None;
    }
    // In the order they run, which is from the bottom.
    stack.reverse();
    let kept =
        stack.iter().map(|&test| kept(plan, test, base, stats)).collect::<Option<Vec<_>>>()?;
    let mut order: Vec<usize> = (0..stack.len()).collect();
    order.sort_by(|&a, &b| kept[a].total_cmp(&kept[b]));
    if order.iter().enumerate().all(|(place, &was)| place == was) {
        return None;
    }
    let mut input = base;
    for was in order {
        let mut node = plan.node(stack[was]).clone();
        if let Node::Join { left, .. } = &mut node {
            *left = input;
        }
        let span = plan.node_span(stack[was]);
        input = plan.add_node_at(node, span);
    }
    Some(input)
}

/// The share of `base`'s rows the semi or anti join `test` would keep if `base` were its left side.
///
/// Asked of the stack's base and not of each test's own left side, because the tests are about to
/// be put over each other in some other order and the base is what every one of them reads its key
/// out of whichever order that is.
fn kept(plan: &Plan, test: NodeRef, base: NodeRef, stats: &Facts) -> Option<f64> {
    let Node::Join { right, kind, conditions, .. } = *plan.node(test) else {
        return None;
    };
    let mut inside = Vec::new();
    indices(plan, right, &mut inside);
    let mut key = None;
    let mut crossing = 1.0;
    let mut itself = false;
    for &condition in plan.expr_list(conditions) {
        let (mut reads_right, mut reads_left) = (false, false);
        plan.read_columns(condition, &mut |_, binding| {
            if inside.contains(&binding.table) {
                reads_right = true;
            } else {
                reads_left = true;
            }
        });
        if !reads_right || !reads_left {
            return None;
        }
        let Some((op, near, far)) = compared(plan, condition, &inside) else {
            crossing *= KEPT_BY_A_CONDITION;
            continue;
        };
        let same = same_column(plan, right, near, base, far);
        match op {
            CompareOp::Equal if key.is_none() => key = Some((near, same)),
            CompareOp::NotEqual => itself |= same,
            _ => crossing *= KEPT_BY_A_CONDITION,
        }
    }
    // Only a test over the rows of the same group of the same table.
    let (near, true) = key? else { return None };
    let share = filtered(plan, right, stats)? * crossing;
    let group = estimate::unfiltered(plan, right, stats)
        .read(CARDINALITY)
        .copied()
        .zip(estimate::stated(plan, near, stats).read(DISTINCT).copied())
        .filter(|&(_, distinct)| distinct > 0);
    #[expect(clippy::cast_precision_loss, reason = "a share of rows, not a count")]
    let others = match group {
        Some((rows, distinct)) => rows as f64 / distinct as f64,
        // Nobody counted the key, so the least a group can be, one row besides the row itself.
        None => 2.0,
    } - if itself { 1.0 } else { 0.0 };
    let missed = (1.0 - share).powf(others.max(0.0));
    Some(if kind == JoinKind::Anti { missed } else { 1.0 - missed })
}

/// A comparison between a column of the side `inside` holds and a column of the other side, as the
/// operator with the inside column on the left, the inside column and the other.
fn compared(
    plan: &Plan,
    condition: ExprRef,
    inside: &[u32],
) -> Option<(CompareOp, ColumnBinding, ColumnBinding)> {
    let Expr::Compare { op, left, right } = *plan.expr(condition) else { return None };
    let (&Expr::Column(left), &Expr::Column(right)) = (plan.expr(left), plan.expr(right)) else {
        return None;
    };
    match (inside.contains(&left.table), inside.contains(&right.table)) {
        (true, false) => Some((op, left, right)),
        (false, true) => Some((op.flip(), right, left)),
        _ => None,
    }
}

/// Whether `near` under `right` and `far` under `left` are one column of one stored table.
fn same_column(
    plan: &Plan,
    right: NodeRef,
    near: ColumnBinding,
    left: NodeRef,
    far: ColumnBinding,
) -> bool {
    let stored = |at: NodeRef, key: ColumnBinding| {
        let (scan, key) = walk::key_origin(plan, at, key)?;
        let Node::Get { catalog, schema, table, columns, .. } = *plan.node(scan) else {
            return None;
        };
        let field = plan.field_list(columns).get(key.column as usize)?;
        Some((plan.string(catalog), plan.string(schema), plan.string(table), field.name.as_str()))
    };
    matches!((stored(right, near), stored(left, far)), (Some(a), Some(b)) if a == b)
}

/// The share of its table's rows a side keeps, where the side is filters over a scan.
fn filtered(plan: &Plan, at: NodeRef, stats: &Facts) -> Option<f64> {
    match *plan.node(at) {
        Node::Get { .. } => Some(1.0),
        Node::Filter { input, predicate } => {
            let conjuncts = match *plan.expr(predicate) {
                Expr::Conjunction { op: ConjunctionOp::And, children } => {
                    plan.expr_list(children).to_vec()
                }
                _ => vec![predicate],
            };
            let under = filtered(plan, input, stats)?;
            Some(
                conjuncts
                    .into_iter()
                    .map(|conjunct| estimate::kept_by(plan, input, conjunct, stats).0)
                    .product::<f64>()
                    * under,
            )
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    use super::ExistsOrder;
    use crate::pass::{Context, Pass};

    fn rewritten(text: &str, context: &Context) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        ExistsOrder.run(&mut plan, context).expect("the pass does not fail");
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The two tests of q21 over the late lines, the `EXISTS` below the `NOT EXISTS`.
    const LINES: &str = concat!(
        "Join ANTI on=[(#2.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#2.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
        "  Join SEMI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#1.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
        "    Get memory.main.lineitem AS l1 #0 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
        "    Get memory.main.lineitem AS l2 #1 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
        "  Filter (#2.3::DATE > #2.2::DATE)::BOOLEAN\n",
        "    Get memory.main.lineitem AS l3 #2 [l_orderkey::BIGINT, l_suppkey::BIGINT, l_commitdate::DATE, l_receiptdate::DATE]\n",
    );

    /// [`LINES`] with the `NOT EXISTS` run first.
    const ANTI_FIRST: &str = concat!(
        "Join SEMI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#1.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
        "  Join ANTI on=[(#2.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#2.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
        "    Get memory.main.lineitem AS l1 #0 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
        "    Filter (#2.3::DATE > #2.2::DATE)::BOOLEAN\n",
        "      Get memory.main.lineitem AS l3 #2 [l_orderkey::BIGINT, l_suppkey::BIGINT, l_commitdate::DATE, l_receiptdate::DATE]\n",
        "  Get memory.main.lineitem AS l2 #1 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
    );

    #[test]
    fn the_test_that_keeps_less_runs_first() {
        assert_eq!(rewritten(LINES, &Context::new()), ANTI_FIRST);
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        assert_eq!(rewritten(ANTI_FIRST, &Context::new()), ANTI_FIRST);
    }

    #[test]
    fn a_test_over_another_table_is_not_estimated() {
        // The `EXISTS` is over orders now, which is a parent and not the line's own group, so there
        // is nothing to set the two against each other with.
        let text = LINES
            .replace(
                "AS l2 #1 [l_orderkey::BIGINT, l_suppkey::BIGINT]",
                "AS l2 #1 [o_orderkey::BIGINT, o_custkey::BIGINT]",
            )
            .replace("memory.main.lineitem AS l2", "memory.main.orders AS l2");
        assert_eq!(rewritten(&text, &Context::new()), text);
    }

    #[test]
    fn a_filter_between_the_tests_ends_the_stack() {
        let text = concat!(
            "Join ANTI on=[(#2.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#2.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
            "  Filter (#0.1::BIGINT > 5::BIGINT)::BOOLEAN\n",
            "    Join SEMI on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, (#1.1::BIGINT <> #0.1::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.lineitem AS l1 #0 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
            "      Get memory.main.lineitem AS l2 #1 [l_orderkey::BIGINT, l_suppkey::BIGINT]\n",
            "  Filter (#2.3::DATE > #2.2::DATE)::BOOLEAN\n",
            "    Get memory.main.lineitem AS l3 #2 [l_orderkey::BIGINT, l_suppkey::BIGINT, l_commitdate::DATE, l_receiptdate::DATE]\n",
        );
        assert_eq!(rewritten(text, &Context::new()), text);
    }

    #[test]
    fn the_rule_turns_it_off() {
        let mut rules = Rules::new();
        rules.set(Rule::FilterOrder, false);
        let mut context = Context::new();
        context.govern(rules);
        assert_eq!(rewritten(LINES, &context), LINES);
    }
}
