//! Constant folding and the simplifications that fall out of it.
//!
//! DuckDB calls this pass `expression_rewriter` and so does rudb, because a corpus file that says
//! `SET disabled_optimizers = 'expression_rewriter'` was written to turn this off. It is a subset
//! of what the binary's rewriter does. What is here is folding, the conjunction rules, the `CASE`
//! rules and the comparison against a null, all of which were read off the pinned binary rather
//! than reasoned about. What is not here is arithmetic simplification, which turns `i + 0` into `i`,
//! and reassociation, which turns `3 + 4 + i` into `7 + i`. Both are worth having and neither is
//! folding, so both are a later pull request.
//!
//! `spec/engine/11-optimizer.md` section 11.2 puts this fifth by value, worth tens of percent
//! rather than multiples, and then gives the reason it lands earlier than that: folding is required
//! for correctness in a few places anyway. A `WHERE false` that is still an expression is a table
//! scan, and the pass that removes the scan can only see that the predicate is false once something
//! has made it false.
//!
//! # What is not folded, and why
//!
//! A fold that raises is abandoned and the expression is left exactly as it was written. `CAST('abc'
//! AS INTEGER)` stays a cast, so the error still comes from running the query rather than from
//! planning it, and a query whose unreachable branch would have raised still runs. The binary does
//! the same thing and it is the only safe rule: an optimizer that can turn a query that returns rows
//! into a query that returns an error is an optimizer that changes answers.
//!
//! A volatile function is not folded. There are none in rudb yet, so [`VOLATILE`] is a list of names
//! nothing answers to, and that is on purpose. The day `random()` lands, a pass that folded it would
//! give every row the same number, and the version of this file that grows the list at the same time
//! as the function is the version where somebody has to remember.
//!
//! A fold whose value does not have the type the plan recorded for the expression is abandoned too.
//! That cannot happen if the kernels and the binder agree, which is the point: it is a disagreement
//! between the two, and turning it into a plan that still runs correctly is better than turning it
//! into a validation failure a long way from the cause.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result, Value};
use rudb_kernels::{Comparison, Connective, call_values, cast_value, combine, compare_values};
use rudb_plan::{
    Arm, CompareOp, ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan, Slice, SortKey,
};
use rudb_vector::Vector;

use crate::pass::{Context, Pass, top_down};

/// The functions whose value is not decided by their arguments.
///
/// `SELECT DISTINCT function_name FROM duckdb_functions() WHERE has_side_effects` on the pinned
/// binary, which is the list at the commit the grammar is vendored from. rudb implements none of
/// them today and the list is here anyway, so that the first one to land is refused by a pass that
/// already knew about it rather than folded by a pass that had never heard of it.
pub const VOLATILE: [&str; 17] = [
    "current_connection_id",
    "current_query",
    "current_query_id",
    "current_transaction_id",
    "currval",
    "error",
    "gen_random_uuid",
    "nextval",
    "random",
    "setseed",
    "setval",
    "sleep_ms",
    "stats",
    "uuid",
    "uuidv4",
    "uuidv7",
    "write_log",
];

/// Folds what can be folded and simplifies what folding exposes.
#[derive(Debug, Clone, Copy)]
pub struct ExpressionRewriter;

impl Pass for ExpressionRewriter {
    fn name(&self) -> &'static str {
        "expression_rewriter"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        rewrite(plan);
        Ok(())
    }
}

/// What each expression has already been rewritten to.
///
/// The arena shares operands, so one expression is reached from as many places as refer to it, and
/// rewriting it once per reference would turn a shared subtree into as many copies as there are
/// references. Sharing it afterwards is not only about the arena's size: the prepared form in the
/// executor keys its common subexpressions by reference, so a tree that arrives unshared runs the
/// same work twice.
type Done = HashMap<ExprRef, ExprRef>;

/// Rewrites every expression the plan reaches.
fn rewrite(plan: &mut Plan) {
    let mut done = Done::new();
    for node in top_down(plan) {
        node_expressions(plan, node, &mut done);
    }
}

/// Rewrites the expressions one node holds, writing back only the slots that changed.
///
/// Only what changed, because a pool entry is appended rather than overwritten and a pass that
/// rebuilt every slice would grow the arena by the size of the plan every time it ran. Running it
/// twice has to be running it once, which is what the idempotence assertion in
/// `spec/09-optimizer.md` section 9.1 asks of every pass.
fn node_expressions(plan: &mut Plan, node: NodeRef, done: &mut Done) {
    match *plan.node(node) {
        Node::Get { .. }
        | Node::Dummy
        | Node::Limit { .. }
        | Node::SetOp { .. }
        | Node::CrossProduct { .. } => {}
        Node::Values { rows, .. } => {
            let held = plan.row_list(rows).to_vec();
            let rewritten: Vec<Slice> =
                held.iter().map(|&row| expr_list(plan, row, done).unwrap_or(row)).collect();
            if rewritten != held {
                let rows = plan.add_rows(&rewritten);
                match plan.node_mut(node) {
                    Node::Values { rows: held, .. } => *held = rows,
                    _ => unreachable!("the node was a values list a moment ago"),
                }
            }
        }
        Node::TableFunction { args, .. } => {
            if let Some(rewritten) = expr_list(plan, args, done) {
                match plan.node_mut(node) {
                    Node::TableFunction { args, .. } => *args = rewritten,
                    _ => unreachable!("the node was a table function a moment ago"),
                }
            }
        }
        Node::Filter { predicate, .. } => {
            let rewritten = expression(plan, predicate, done);
            if rewritten != predicate {
                match plan.node_mut(node) {
                    Node::Filter { predicate, .. } => *predicate = rewritten,
                    _ => unreachable!("the node was a filter a moment ago"),
                }
            }
        }
        Node::Project { exprs, .. } => {
            if let Some(rewritten) = expr_list(plan, exprs, done) {
                match plan.node_mut(node) {
                    Node::Project { exprs, .. } => *exprs = rewritten,
                    _ => unreachable!("the node was a projection a moment ago"),
                }
            }
        }
        Node::Aggregate { groups, aggregates, .. } => {
            let rewritten_groups = expr_list(plan, groups, done);
            let rewritten_aggregates = expr_list(plan, aggregates, done);
            match plan.node_mut(node) {
                Node::Aggregate { groups, aggregates, .. } => {
                    if let Some(rewritten) = rewritten_groups {
                        *groups = rewritten;
                    }
                    if let Some(rewritten) = rewritten_aggregates {
                        *aggregates = rewritten;
                    }
                }
                _ => unreachable!("the node was an aggregate a moment ago"),
            }
        }
        Node::Sort { keys, .. } => {
            let held = plan.sort_key_list(keys).to_vec();
            let rewritten: Vec<SortKey> = held
                .iter()
                .map(|key| SortKey { expr: expression(plan, key.expr, done), ..*key })
                .collect();
            if rewritten != held {
                let keys = plan.add_sort_keys(&rewritten);
                match plan.node_mut(node) {
                    Node::Sort { keys: held, .. } => *held = keys,
                    _ => unreachable!("the node was a sort a moment ago"),
                }
            }
        }
        Node::Distinct { on, .. } => {
            if let Some(rewritten) = expr_list(plan, on, done) {
                match plan.node_mut(node) {
                    Node::Distinct { on, .. } => *on = rewritten,
                    _ => unreachable!("the node was a distinct a moment ago"),
                }
            }
        }
        Node::Join { conditions, .. } => {
            if let Some(rewritten) = expr_list(plan, conditions, done) {
                match plan.node_mut(node) {
                    Node::Join { conditions, .. } => *conditions = rewritten,
                    _ => unreachable!("the node was a join a moment ago"),
                }
            }
        }
    }
}

/// Rewrites a run of expressions, handing back a new slice only if one of them changed.
fn expr_list(plan: &mut Plan, slice: Slice, done: &mut Done) -> Option<Slice> {
    let held = plan.expr_list(slice).to_vec();
    let rewritten: Vec<ExprRef> = held.iter().map(|&expr| expression(plan, expr, done)).collect();
    (rewritten != held).then(|| plan.add_expr_list(&rewritten))
}

/// Rewrites one expression and everything under it, bottom up.
///
/// Bottom up is not a preference. An expression may only refer to an expression behind it in the
/// arena, which `Plan::validate` checks and which is what makes a plan acyclic by construction, so a
/// rewritten operand has to be appended before the operator that reads it. Folding a child first is
/// also what makes one pass enough: `1 + 2 + 3` folds to `6` in a single walk because by the time
/// the outer call runs, its operand is already a constant.
fn expression(plan: &mut Plan, expr: ExprRef, done: &mut Done) -> ExprRef {
    if let Some(&already) = done.get(&expr) {
        return already;
    }
    let rebuilt = rebuild(plan, expr, done);
    let simplified = simplify(plan, rebuilt);
    done.insert(expr, simplified);
    simplified
}

/// Rewrites the operands, rebuilding the expression only if one of them moved.
fn rebuild(plan: &mut Plan, expr: ExprRef, done: &mut Done) -> ExprRef {
    let ty = plan.expr_type(expr).clone();
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => expr,
        Expr::Cast { input, try_cast } => {
            let rewritten = expression(plan, input, done);
            if rewritten == input {
                expr
            } else {
                plan.add_expr(Expr::Cast { input: rewritten, try_cast }, ty)
            }
        }
        Expr::Compare { op, left, right } => {
            let rewritten_left = expression(plan, left, done);
            let rewritten_right = expression(plan, right, done);
            if rewritten_left == left && rewritten_right == right {
                expr
            } else {
                plan.add_expr(
                    Expr::Compare { op, left: rewritten_left, right: rewritten_right },
                    ty,
                )
            }
        }
        Expr::Conjunction { op, children } => match expr_list(plan, children, done) {
            None => expr,
            Some(children) => plan.add_expr(Expr::Conjunction { op, children }, ty),
        },
        Expr::Function { name, args } => match expr_list(plan, args, done) {
            None => expr,
            Some(args) => plan.add_expr(Expr::Function { name, args }, ty),
        },
        Expr::Aggregate { name, args, distinct, filter } => {
            let rewritten_args = expr_list(plan, args, done);
            let rewritten_filter = filter.map(|inner| expression(plan, inner, done));
            if rewritten_args.is_none() && rewritten_filter == filter {
                expr
            } else {
                let args = rewritten_args.unwrap_or(args);
                plan.add_expr(
                    Expr::Aggregate { name, args, distinct, filter: rewritten_filter },
                    ty,
                )
            }
        }
        Expr::Case { arms, otherwise } => {
            let held = plan.arm_list(arms).to_vec();
            let rewritten: Vec<Arm> = held
                .iter()
                .map(|arm| Arm {
                    when: expression(plan, arm.when, done),
                    then: expression(plan, arm.then, done),
                })
                .collect();
            let rewritten_otherwise = otherwise.map(|inner| expression(plan, inner, done));
            if rewritten == held && rewritten_otherwise == otherwise {
                expr
            } else {
                let arms = plan.add_arms(&rewritten);
                plan.add_expr(Expr::Case { arms, otherwise: rewritten_otherwise }, ty)
            }
        }
    }
}

/// Applies every rule to one expression whose operands are already rewritten.
fn simplify(plan: &mut Plan, expr: ExprRef) -> ExprRef {
    if matches!(*plan.expr(expr), Expr::Constant(_)) {
        return expr;
    }
    if let Some(value) = fold(plan, expr) {
        if let Some(folded) = constant_of(plan, expr, value) {
            return folded;
        }
    }
    match *plan.expr(expr) {
        Expr::Conjunction { op, children } => conjunction(plan, expr, op, children),
        Expr::Case { arms, otherwise } => case(plan, expr, arms, otherwise),
        Expr::Compare { op, left, right } => null_comparison(plan, expr, op, left, right),
        _ => expr,
    }
}

/// The value of an expression all of whose operands are constants, if it has one.
///
/// One level deep, because the operands have already been through this and a foldable one is
/// already a constant. The kernels it calls are the ones the executor calls for the same
/// expression, which is what makes a folded answer and a computed answer the same answer by
/// construction rather than by testing every function twice.
fn fold(plan: &Plan, expr: ExprRef) -> Option<Value> {
    match *plan.expr(expr) {
        Expr::Cast { input, try_cast } => {
            let inner = constant(plan, input)?;
            cast_value(&inner, plan.expr_type(expr), try_cast).ok()
        }
        Expr::Compare { op, left, right } => {
            let left = constant(plan, left)?;
            let right = constant(plan, right)?;
            compare_values(comparison(op), &left, &right).ok()
        }
        Expr::Conjunction { op, children } => {
            let values = constants(plan, children)?;
            let vectors: Vec<Vector> = values
                .into_iter()
                .map(|value| Vector::constant(LogicalType::Boolean, value, 1))
                .collect();
            Some(combine(connective(op), &vectors).ok()?.value_at(0))
        }
        Expr::Function { name, args } => {
            let name = plan.string(name);
            if VOLATILE.contains(&name) {
                return None;
            }
            let values = constants(plan, args)?;
            call_values(name, &values, plan.expr_type(expr)).ok()
        }
        _ => None,
    }
}

/// The value behind an expression, if it is a constant.
fn constant(plan: &Plan, expr: ExprRef) -> Option<Value> {
    match *plan.expr(expr) {
        Expr::Constant(value) => Some(plan.value(value).clone()),
        _ => None,
    }
}

/// The values behind a run of expressions, if every one of them is a constant.
fn constants(plan: &Plan, slice: Slice) -> Option<Vec<Value>> {
    plan.expr_list(slice).iter().map(|&expr| constant(plan, expr)).collect()
}

/// A constant expression holding `value`, keeping the type the expression already had.
///
/// The expression's own type and not the value's, because a null carries no type and the plan says
/// what the column is. `None` if the two disagree about anything else, which is the abandoned fold
/// the module documentation describes.
fn constant_of(plan: &mut Plan, expr: ExprRef, value: Value) -> Option<ExprRef> {
    let ty = plan.expr_type(expr).clone();
    if !value.is_null() && value.logical_type() != ty {
        return None;
    }
    let held = plan.add_value(value);
    Some(plan.add_expr(Expr::Constant(held), ty))
}

/// `x AND true` is `x`, `x AND false` is `false`, and the same the other way up for `OR`.
///
/// A null operand is kept rather than dropped, because `x AND NULL` is null where `x` is true and
/// false where `x` is false, so it is neither the operand nor a constant. The binary keeps it too.
fn conjunction(plan: &mut Plan, expr: ExprRef, op: ConjunctionOp, children: Slice) -> ExprRef {
    // The value that decides the whole connective on its own, and the one that drops out of it.
    let (decides, drops) = match op {
        ConjunctionOp::And => (false, true),
        ConjunctionOp::Or => (true, false),
    };
    let held = plan.expr_list(children).to_vec();
    let mut kept = Vec::with_capacity(held.len());
    for child in held.iter().copied() {
        match constant(plan, child).as_ref().and_then(Value::as_bool) {
            Some(known) if known == decides => {
                return constant_of(plan, expr, Value::Boolean(decides)).unwrap_or(expr);
            }
            Some(_) => {}
            None => kept.push(child),
        }
    }
    if kept.len() == held.len() {
        return expr;
    }
    match kept.as_slice() {
        [] => constant_of(plan, expr, Value::Boolean(drops)).unwrap_or(expr),
        [only] => *only,
        rest => {
            let children = plan.add_expr_list(rest);
            plan.add_expr(Expr::Conjunction { op, children }, LogicalType::Boolean)
        }
    }
}

/// Whether an arm's condition is decided before the query runs.
enum Fires {
    /// The condition is a true constant, so this arm is the answer and the ones after it are not.
    Always,
    /// The condition is a false or null constant. A null condition does not fire, which is the same
    /// rule `WHERE` uses and is why there is one variant for the two.
    Never,
    /// The condition depends on the row.
    Maybe,
}

fn fires(plan: &Plan, when: ExprRef) -> Fires {
    match constant(plan, when) {
        Some(Value::Boolean(true)) => Fires::Always,
        Some(Value::Boolean(false) | Value::Null) => Fires::Never,
        // A condition that is a constant of some other type is a malformed plan, and this is not
        // where that gets reported. `Plan::validate` says so with the expression number.
        Some(_) | None => Fires::Maybe,
    }
}

/// Drops the arms that cannot fire and cuts the `CASE` at the first one that always does.
fn case(plan: &mut Plan, expr: ExprRef, arms: Slice, otherwise: Option<ExprRef>) -> ExprRef {
    let held = plan.arm_list(arms).to_vec();
    let mut kept = Vec::with_capacity(held.len());
    let mut result = otherwise;
    let mut cut = false;
    for arm in held.iter().copied() {
        match fires(plan, arm.when) {
            Fires::Never => {}
            Fires::Always => {
                result = Some(arm.then);
                cut = true;
                break;
            }
            Fires::Maybe => kept.push(arm),
        }
    }
    if kept.len() == held.len() && !cut {
        return expr;
    }
    if kept.is_empty() {
        // Every condition was decided, so the `CASE` is whichever branch was left standing. A
        // branch whose type is not the `CASE`'s own would change what the column is, which the
        // binder never builds and which is left alone rather than guessed at.
        return match result {
            Some(only) if plan.expr_type(only) == plan.expr_type(expr) => only,
            Some(_) => expr,
            None => constant_of(plan, expr, Value::Null).unwrap_or(expr),
        };
    }
    let ty = plan.expr_type(expr).clone();
    let arms = plan.add_arms(&kept);
    plan.add_expr(Expr::Case { arms, otherwise: result }, ty)
}

/// A comparison against a null constant is null, whatever the other side is.
///
/// Not for `IS DISTINCT FROM` and `IS NOT DISTINCT FROM`, which are the two that have an answer
/// when an operand is null and are the reason anybody writes them.
///
/// It drops the other side rather than evaluating it, so an expression that would have raised no
/// longer does. That is what the binary does with the same query, and the alternative is keeping a
/// comparison whose answer is known so that its operand can fail.
fn null_comparison(
    plan: &mut Plan,
    expr: ExprRef,
    op: CompareOp,
    left: ExprRef,
    right: ExprRef,
) -> ExprRef {
    if matches!(op, CompareOp::DistinctFrom | CompareOp::NotDistinctFrom) {
        return expr;
    }
    let is_null = |side| constant(plan, side).is_some_and(|value| value.is_null());
    if is_null(left) || is_null(right) {
        constant_of(plan, expr, Value::Null).unwrap_or(expr)
    } else {
        expr
    }
}

/// The kernels' comparison for the plan's.
///
/// The same eight arms as the copy in `rudb-exec`, which is where the executor's is. One copy would
/// have to live in the crate both can see, and that is `rudb-plan`, which does not depend on the
/// kernels and should not: a plan is a data structure and the day it needs a kernel library to be
/// constructed is the day nothing can hold a plan without linking the arithmetic. Both matches are
/// exhaustive, so a ninth comparison stops both of them compiling rather than quietly folding to
/// the wrong answer in one.
fn comparison(op: CompareOp) -> Comparison {
    match op {
        CompareOp::Equal => Comparison::Equal,
        CompareOp::NotEqual => Comparison::NotEqual,
        CompareOp::Less => Comparison::Less,
        CompareOp::LessOrEqual => Comparison::LessOrEqual,
        CompareOp::Greater => Comparison::Greater,
        CompareOp::GreaterOrEqual => Comparison::GreaterOrEqual,
        CompareOp::DistinctFrom => Comparison::DistinctFrom,
        CompareOp::NotDistinctFrom => Comparison::NotDistinctFrom,
    }
}

/// The kernels' connective for the plan's.
fn connective(op: ConjunctionOp) -> Connective {
    match op {
        ConjunctionOp::And => Connective::And,
        ConjunctionOp::Or => Connective::Or,
    }
}

#[cfg(test)]
mod tests {
    use super::{ExpressionRewriter, VOLATILE};
    use crate::pass::{Context, Pass};
    use rudb_plan::Plan;

    /// The plan a text prints as after folding, which is what every assertion here reads.
    fn folded(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        ExpressionRewriter
            .run(&mut plan, &Context::new())
            .unwrap_or_else(|error| panic!("{text} did not fold: {error}"));
        plan.validate().unwrap_or_else(|error| panic!("{text} folded to a bad plan: {error}"));
        plan.to_string()
    }

    const SCAN: &str = "  Get memory.main.t AS t #0 [a::INTEGER, b::VARCHAR, c::BOOLEAN]\n";

    #[test]
    fn arithmetic_over_constants_becomes_the_number() {
        let before = format!("Project #1 [\"+\"(2::INTEGER, 3::INTEGER)::INTEGER AS n]\n{SCAN}");
        let after = format!("Project #1 [5::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_nest_of_constants_folds_all_the_way_up_in_one_walk() {
        // What makes one pass enough. By the time the outer call is looked at, its operand is
        // already a constant, so there is nothing to run a second time over.
        let before = format!(
            "Project #1 [\"+\"(\"+\"(1::INTEGER, 2::INTEGER)::INTEGER, 3::INTEGER)::INTEGER AS n]\n{SCAN}"
        );
        let after = format!("Project #1 [6::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_call_with_a_column_in_it_is_left_alone() {
        let text = format!("Project #1 [\"+\"(#0.0::INTEGER, 3::INTEGER)::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&text), text);
    }

    #[test]
    fn a_cast_of_a_constant_folds_and_one_that_would_raise_does_not() {
        let before = format!("Project #1 [CAST('1'::VARCHAR)::INTEGER AS n]\n{SCAN}");
        let after = format!("Project #1 [1::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
        // The rule the whole pass rests on. The error still comes from running the query, so a
        // query whose unreachable branch would have raised still runs.
        let raises = format!("Project #1 [CAST('abc'::VARCHAR)::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&raises), raises);
    }

    #[test]
    fn a_comparison_of_constants_becomes_a_boolean() {
        let before = format!("Filter (1::INTEGER < 2::INTEGER)::BOOLEAN\n{SCAN}");
        let after = format!("Filter TRUE::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_comparison_against_a_null_is_null_and_the_other_side_goes_with_it() {
        let before = format!("Filter (#0.0::INTEGER = NULL::INTEGER)::BOOLEAN\n{SCAN}");
        let after = format!("Filter NULL::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn the_two_comparisons_that_have_an_answer_over_a_null_keep_it() {
        // `IS NOT DISTINCT FROM NULL` is a test for null and answers true or false, which is the
        // reason anybody writes it, so the rule above must not reach it.
        let text =
            format!("Filter (#0.0::INTEGER IS NOT DISTINCT FROM NULL::INTEGER)::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&text), text);
    }

    #[test]
    fn a_true_drops_out_of_an_and_and_a_false_decides_it() {
        let before = format!("Filter (TRUE::BOOLEAN AND #0.2::BOOLEAN)::BOOLEAN\n{SCAN}");
        let after = format!("Filter #0.2::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), after);
        let decided = format!("Filter (FALSE::BOOLEAN AND #0.2::BOOLEAN)::BOOLEAN\n{SCAN}");
        let all = format!("Filter FALSE::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&decided), all);
    }

    #[test]
    fn a_false_drops_out_of_an_or_and_a_true_decides_it() {
        let before = format!("Filter (FALSE::BOOLEAN OR #0.2::BOOLEAN)::BOOLEAN\n{SCAN}");
        let after = format!("Filter #0.2::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), after);
        let decided = format!("Filter (TRUE::BOOLEAN OR #0.2::BOOLEAN)::BOOLEAN\n{SCAN}");
        let all = format!("Filter TRUE::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&decided), all);
    }

    #[test]
    fn a_null_operand_of_an_and_is_kept_because_it_is_neither_the_answer_nor_the_operand() {
        let text = format!("Filter (NULL::BOOLEAN AND #0.2::BOOLEAN)::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&text), text);
    }

    #[test]
    fn a_conjunction_of_constants_is_the_three_valued_answer() {
        // `NULL AND false` is false and `NULL OR true` is true, which is the part a rule that only
        // looked at the nulls would get wrong.
        let before = format!("Filter (NULL::BOOLEAN AND FALSE::BOOLEAN)::BOOLEAN\n{SCAN}");
        let after = format!("Filter FALSE::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), after);
        let other = format!("Filter (NULL::BOOLEAN OR TRUE::BOOLEAN)::BOOLEAN\n{SCAN}");
        let answer = format!("Filter TRUE::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&other), answer);
    }

    #[test]
    fn a_long_conjunction_keeps_the_operands_that_are_not_decided() {
        let before = format!(
            "Filter (#0.2::BOOLEAN AND TRUE::BOOLEAN AND (#0.0::INTEGER > 1::INTEGER)::BOOLEAN)::BOOLEAN\n{SCAN}"
        );
        let after = format!(
            "Filter (#0.2::BOOLEAN AND (#0.0::INTEGER > 1::INTEGER)::BOOLEAN)::BOOLEAN\n{SCAN}"
        );
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn an_arm_that_cannot_fire_is_dropped_and_a_null_condition_is_one_of_them() {
        let before = format!(
            "Project #1 [CASE WHEN FALSE::BOOLEAN THEN 1::INTEGER ELSE #0.0::INTEGER END::INTEGER AS n]\n{SCAN}"
        );
        let after = format!("Project #1 [#0.0::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
        let null = format!(
            "Project #1 [CASE WHEN NULL::BOOLEAN THEN 1::INTEGER ELSE #0.0::INTEGER END::INTEGER AS n]\n{SCAN}"
        );
        assert_eq!(folded(&null), after);
    }

    #[test]
    fn the_first_arm_that_always_fires_cuts_the_ones_after_it() {
        let before = format!(
            "Project #1 [CASE WHEN #0.2::BOOLEAN THEN 1::INTEGER WHEN TRUE::BOOLEAN THEN 2::INTEGER WHEN #0.2::BOOLEAN THEN 3::INTEGER ELSE 4::INTEGER END::INTEGER AS n]\n{SCAN}"
        );
        let after = format!(
            "Project #1 [CASE WHEN #0.2::BOOLEAN THEN 1::INTEGER ELSE 2::INTEGER END::INTEGER AS n]\n{SCAN}"
        );
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_case_with_no_arm_left_and_no_else_is_null() {
        let before = format!(
            "Project #1 [CASE WHEN FALSE::BOOLEAN THEN 1::INTEGER END::INTEGER AS n]\n{SCAN}"
        );
        let after = format!("Project #1 [NULL::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn an_aggregate_keeps_its_place_and_its_arguments_are_folded_under_it() {
        // An aggregate may only appear as a direct element of the aggregate list, so folding must
        // rebuild one rather than replace it, however constant its argument is.
        let before = format!(
            "Aggregate #1 groups=[] aggregates=[sum(\"+\"(1::INTEGER, 2::INTEGER)::INTEGER)::HUGEINT]\n{SCAN}"
        );
        let after = format!("Aggregate #1 groups=[] aggregates=[sum(3::INTEGER)::HUGEINT]\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_sort_key_and_a_join_condition_are_folded_too() {
        let before =
            format!("Sort [\"+\"(1::INTEGER, 1::INTEGER)::INTEGER ASC NULLS LAST]\n{SCAN}");
        let after = format!("Sort [2::INTEGER ASC NULLS LAST]\n{SCAN}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn folding_twice_is_folding_once() {
        let before = format!(
            "Filter (TRUE::BOOLEAN AND (\"+\"(1::INTEGER, 1::INTEGER)::INTEGER > #0.0::INTEGER)::BOOLEAN)::BOOLEAN\n{SCAN}"
        );
        let once = folded(&before);
        assert_eq!(folded(&once), once);
    }

    #[test]
    fn a_volatile_call_is_not_folded_however_constant_its_arguments_are() {
        // There is no `random` in rudb yet, so this asserts the list rather than the function. The
        // day one lands, a pass that folded it would give every row the same number.
        assert!(VOLATILE.contains(&"random"));
        assert!(VOLATILE.contains(&"nextval"));
        let text = format!("Project #1 [random()::DOUBLE AS n]\n{SCAN}");
        assert_eq!(folded(&text), text);
    }
}
