//! Walking one expression, without deciding what the walk is for.
//!
//! Four things every pass here ends up needing: rewrite the operands, list the columns, ask whether
//! asking twice can give two answers, and ask whether two expressions are the same expression. None
//! of them is interesting and all of them have a match arm per variant, so a variant added to `Expr`
//! and forgotten about is a compile error in one file instead of four.
//!
//! Folding replaces an operand with its value where it has one, filter pushdown replaces a column
//! reference with whatever the operator below computes that column from, and transitive predicates
//! replace a column reference with the column an equality says it equals. Everything except that one
//! sentence is the same code in all three: visit each operand, and if any of them moved, append a new
//! expression carrying the operands that moved and the type the plan already recorded.
//!
//! Appending rather than editing in place is not a style choice. An expression may only refer to an
//! expression behind it in the arena, which [`Plan::validate`] checks and which is what makes a plan
//! acyclic by construction, so a rewritten operand has to be appended before the operator that reads
//! it can be.
//!
//! [`Plan::validate`]: rudb_plan::Plan::validate

use rudb_plan::{Arm, ColumnBinding, Expr, ExprRef, Plan, Slice};

use crate::fold::VOLATILE;

/// Rewrites the operands of one expression, rebuilding it only if one of them moved.
///
/// One level deep, and `child` decides whether to go further. The two callers want different
/// answers there: folding walks to the leaves, and filter pushdown stops at a column reference,
/// since what it puts in place of one is already written in terms of the operator below.
///
/// The type of the rebuilt expression is the type of the one it replaces. A rewrite that changes the
/// type of an operand has to leave the expression alone instead, because the plan records a type per
/// expression and a caller that guessed a new one would be guessing about a cast.
pub(crate) fn rebuild(
    plan: &mut Plan,
    expr: ExprRef,
    child: &mut impl FnMut(&mut Plan, ExprRef) -> ExprRef,
) -> ExprRef {
    let ty = plan.expr_type(expr).clone();
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => expr,
        Expr::Cast { input, try_cast } => {
            let rewritten = child(plan, input);
            if rewritten == input {
                expr
            } else {
                plan.add_expr(Expr::Cast { input: rewritten, try_cast }, ty)
            }
        }
        Expr::Compare { op, left, right } => {
            let rewritten_left = child(plan, left);
            let rewritten_right = child(plan, right);
            if rewritten_left == left && rewritten_right == right {
                expr
            } else {
                plan.add_expr(
                    Expr::Compare { op, left: rewritten_left, right: rewritten_right },
                    ty,
                )
            }
        }
        Expr::Conjunction { op, children } => match list(plan, children, child) {
            None => expr,
            Some(children) => plan.add_expr(Expr::Conjunction { op, children }, ty),
        },
        Expr::Function { name, args } => match list(plan, args, child) {
            None => expr,
            Some(args) => plan.add_expr(Expr::Function { name, args }, ty),
        },
        Expr::Aggregate { name, args, distinct, filter } => {
            let rewritten_args = list(plan, args, child);
            let rewritten_filter = filter.map(|inner| child(plan, inner));
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
                .map(|arm| Arm { when: child(plan, arm.when), then: child(plan, arm.then) })
                .collect();
            let rewritten_otherwise = otherwise.map(|inner| child(plan, inner));
            if rewritten == held && rewritten_otherwise == otherwise {
                expr
            } else {
                let arms = plan.add_arms(&rewritten);
                plan.add_expr(Expr::Case { arms, otherwise: rewritten_otherwise }, ty)
            }
        }
    }
}

/// Rewrites a run of expressions, handing back a new slice only if one of them moved.
///
/// Nothing is appended when nothing moved, so a pass that finds no work adds no slices to the pool
/// and a plan it has already run over is left exactly as it was.
pub(crate) fn list(
    plan: &mut Plan,
    slice: Slice,
    child: &mut impl FnMut(&mut Plan, ExprRef) -> ExprRef,
) -> Option<Slice> {
    let held = plan.expr_list(slice).to_vec();
    let rewritten: Vec<ExprRef> = held.iter().map(|&expr| child(plan, expr)).collect();
    (rewritten != held).then(|| plan.add_expr_list(&rewritten))
}

/// Calls `found` for every column `expr` reads.
pub(crate) fn columns(plan: &Plan, expr: ExprRef, found: &mut impl FnMut(ColumnBinding)) {
    match *plan.expr(expr) {
        Expr::Column(binding) => found(binding),
        Expr::Constant(_) => {}
        Expr::Cast { input, .. } => columns(plan, input, found),
        Expr::Compare { left, right, .. } => {
            columns(plan, left, found);
            columns(plan, right, found);
        }
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            for &child in plan.expr_list(children) {
                columns(plan, child, found);
            }
        }
        Expr::Aggregate { args, filter, .. } => {
            for &arg in plan.expr_list(args) {
                columns(plan, arg, found);
            }
            if let Some(inner) = filter {
                columns(plan, inner, found);
            }
        }
        Expr::Case { arms, otherwise } => {
            for arm in plan.arm_list(arms) {
                columns(plan, arm.when, found);
                columns(plan, arm.then, found);
            }
            if let Some(inner) = otherwise {
                columns(plan, inner, found);
            }
        }
    }
}

/// Whether asking for this expression twice can give two answers.
///
/// The list is [`VOLATILE`], which is the one folding refuses to fold and is read from the pinned
/// binary's `duckdb_functions()`. rudb answers to none of those names yet, so this is false for
/// everything today and is here so that the first one to land is refused by a pass that already knew
/// about it rather than copied by a pass that had never heard of it.
///
/// Copying is where it matters. A pass that moves an expression somewhere else is fine either way,
/// and a pass that writes it down twice has turned one call into two.
pub(crate) fn volatile(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => false,
        Expr::Cast { input, .. } => volatile(plan, input),
        Expr::Compare { left, right, .. } => volatile(plan, left) || volatile(plan, right),
        Expr::Conjunction { children, .. } => any_volatile(plan, children),
        Expr::Function { name, args } => {
            VOLATILE.contains(&plan.string(name)) || any_volatile(plan, args)
        }
        Expr::Aggregate { args, filter, .. } => {
            any_volatile(plan, args) || filter.is_some_and(|inner| volatile(plan, inner))
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .any(|arm| volatile(plan, arm.when) || volatile(plan, arm.then))
                || otherwise.is_some_and(|inner| volatile(plan, inner))
        }
    }
}

/// Whether any expression in the run is volatile.
fn any_volatile(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().any(|&expr| volatile(plan, expr))
}

/// Whether the value for a row is decided by that row on its own.
///
/// The question every pass that moves an expression to a different operator has to answer, because
/// an operator is where the set of rows an expression is evaluated over is decided. An elementwise
/// expression does not care which rows it is evaluated beside, so a pass may put it above a filter
/// or below one and get the same answer for the rows that survive. An aggregate is the opposite: it
/// is a value per group, so a group by with one fewer row in a group answers differently.
///
/// This is not the volatility question. `random()` is elementwise and is not safe to copy, and an
/// aggregate is safe to copy and is not elementwise, so a pass that moves a copy of an expression
/// has to ask both. [`volatile`] is the other one.
///
/// It is false for an aggregate today and that is the whole of the list, because an aggregate is
/// the only expression rudb has whose value reads more than one row. A window function is the next
/// one and there is none yet. The binder puts every aggregate in a [`Node::Aggregate`] rather than
/// leaving one in a projection, so nothing a query produces today can make this false, and it is
/// asked anyway: the first pass to believe the invariant without checking it is the pass that
/// pushes a filter through a window function and answers the wrong query.
///
/// [`Node::Aggregate`]: rudb_plan::Node::Aggregate
pub(crate) fn elementwise(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Column(_) | Expr::Constant(_) => true,
        Expr::Aggregate { .. } => false,
        Expr::Cast { input, .. } => elementwise(plan, input),
        Expr::Compare { left, right, .. } => elementwise(plan, left) && elementwise(plan, right),
        Expr::Conjunction { children, .. } | Expr::Function { args: children, .. } => {
            all_elementwise(plan, children)
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .all(|arm| elementwise(plan, arm.when) && elementwise(plan, arm.then))
                && otherwise.is_none_or(|inner| elementwise(plan, inner))
        }
    }
}

/// Whether every expression in the run is elementwise.
fn all_elementwise(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().all(|&expr| elementwise(plan, expr))
}

/// Whether the expression has the same value for every row.
///
/// It reads no column, calls nothing [`volatile`] and holds no aggregate, so its value is decided
/// before the first row is read. That is a wider question than whether it is already a literal:
/// `CAST('abc' AS INTEGER) > 1` is constant and folding leaves it alone, because a fold that raises
/// is abandoned so that the error still comes from running the query.
///
/// An aggregate is false rather than true. It is one value per group, which is constant within a
/// group and not across the input, and the difference between those two is not one this answer can
/// carry, so it says the safe of the two.
pub(crate) fn constant(plan: &Plan, expr: ExprRef) -> bool {
    match *plan.expr(expr) {
        Expr::Constant(_) => true,
        Expr::Column(_) | Expr::Aggregate { .. } => false,
        Expr::Cast { input, .. } => constant(plan, input),
        Expr::Compare { left, right, .. } => constant(plan, left) && constant(plan, right),
        Expr::Conjunction { children, .. } => all_constant(plan, children),
        Expr::Function { name, args } => {
            !VOLATILE.contains(&plan.string(name)) && all_constant(plan, args)
        }
        Expr::Case { arms, otherwise } => {
            plan.arm_list(arms)
                .iter()
                .all(|arm| constant(plan, arm.when) && constant(plan, arm.then))
                && otherwise.is_none_or(|inner| constant(plan, inner))
        }
    }
}

/// Whether every expression in the run has the same value for every row.
fn all_constant(plan: &Plan, slice: Slice) -> bool {
    plan.expr_list(slice).iter().all(|&expr| constant(plan, expr))
}

/// Whether two expressions are the same expression written out.
///
/// The arena does not share anything, so an expression built twice is two references to two copies
/// and `one == other` is only true when both came from the same place. Transitive predicates need
/// the other question, since the predicate it derives is often one the query already had, and adding
/// a second copy of it would make the pass produce a different plan each time it ran.
///
/// A constant compares by value rather than by reference for the same reason, and a function by the
/// name it resolved to rather than by where that name is interned.
pub(crate) fn same(plan: &Plan, one: ExprRef, other: ExprRef) -> bool {
    if one == other {
        return true;
    }
    if plan.expr_type(one) != plan.expr_type(other) {
        return false;
    }
    match (plan.expr(one), plan.expr(other)) {
        (Expr::Column(left), Expr::Column(right)) => left == right,
        (Expr::Constant(left), Expr::Constant(right)) => plan.value(*left) == plan.value(*right),
        (
            Expr::Cast { input: left, try_cast: left_try },
            Expr::Cast { input: right, try_cast: right_try },
        ) => left_try == right_try && same(plan, *left, *right),
        (
            Expr::Compare { op: left_op, left: left_one, right: left_other },
            Expr::Compare { op: right_op, left: right_one, right: right_other },
        ) => {
            left_op == right_op
                && same(plan, *left_one, *right_one)
                && same(plan, *left_other, *right_other)
        }
        (
            Expr::Conjunction { op: left_op, children: left },
            Expr::Conjunction { op: right_op, children: right },
        ) => left_op == right_op && same_list(plan, *left, *right),
        (
            Expr::Function { name: left_name, args: left },
            Expr::Function { name: right_name, args: right },
        ) => plan.string(*left_name) == plan.string(*right_name) && same_list(plan, *left, *right),
        (
            Expr::Aggregate {
                name: left_name,
                args: left,
                distinct: left_distinct,
                filter: left_filter,
            },
            Expr::Aggregate {
                name: right_name,
                args: right,
                distinct: right_distinct,
                filter: right_filter,
            },
        ) => {
            plan.string(*left_name) == plan.string(*right_name)
                && left_distinct == right_distinct
                && same_list(plan, *left, *right)
                && same_option(plan, *left_filter, *right_filter)
        }
        (
            Expr::Case { arms: left, otherwise: left_otherwise },
            Expr::Case { arms: right, otherwise: right_otherwise },
        ) => {
            let (left, right) = (plan.arm_list(*left).to_vec(), plan.arm_list(*right).to_vec());
            let (left_otherwise, right_otherwise) = (*left_otherwise, *right_otherwise);
            left.len() == right.len()
                && left.iter().zip(&right).all(|(one, other)| {
                    same(plan, one.when, other.when) && same(plan, one.then, other.then)
                })
                && same_option(plan, left_otherwise, right_otherwise)
        }
        _ => false,
    }
}

/// Whether two runs of expressions are the same run.
fn same_list(plan: &Plan, one: Slice, other: Slice) -> bool {
    let (one, other) = (plan.expr_list(one).to_vec(), plan.expr_list(other).to_vec());
    one.len() == other.len()
        && one.iter().zip(&other).all(|(&left, &right)| same(plan, left, right))
}

/// Whether two optional expressions are the same, counting absent as the same as absent.
fn same_option(plan: &Plan, one: Option<ExprRef>, other: Option<ExprRef>) -> bool {
    match (one, other) {
        (None, None) => true,
        (Some(left), Some(right)) => same(plan, left, right),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rudb_plan::{Expr, ExprRef, Node, Plan};

    use super::{constant, elementwise, volatile};

    /// The predicate of the filter at the root.
    fn predicate(plan: &Plan) -> ExprRef {
        match *plan.node(plan.root()) {
            Node::Filter { predicate, .. } => predicate,
            _ => panic!("the root is not a filter"),
        }
    }

    /// A plan whose root is a filter holding `text` over a one column scan.
    fn filtered(text: &str) -> Plan {
        let text = format!("Filter {text}\n  Get memory.main.t AS t #0 [a::INTEGER]\n");
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    #[test]
    fn a_column_is_elementwise_and_is_not_constant() {
        let plan = filtered("(#0.0::INTEGER > 1::INTEGER)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(elementwise(&plan, predicate));
        assert!(!constant(&plan, predicate));
    }

    #[test]
    fn a_comparison_of_two_literals_is_both() {
        // Written the way an abandoned fold leaves it, which is where a constant predicate over no
        // column actually comes from once the rewriter has been over the plan.
        let plan = filtered("(1::INTEGER > 2::INTEGER)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(elementwise(&plan, predicate));
        assert!(constant(&plan, predicate));
    }

    #[test]
    fn a_volatile_call_is_elementwise_and_is_not_constant() {
        // The two questions come apart here. `random()` is decided by nothing, which is what makes
        // it not constant, and it is still one call per row, which is what makes it elementwise.
        let plan = filtered("(random()::DOUBLE > 0.5::DOUBLE)::BOOLEAN");
        let predicate = predicate(&plan);
        assert!(volatile(&plan, predicate));
        assert!(elementwise(&plan, predicate));
        assert!(!constant(&plan, predicate));
    }

    #[test]
    fn an_aggregate_is_neither_however_constant_its_arguments_are() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[sum(1::INTEGER)::HUGEINT]\n",
            "  Get memory.main.t AS t #0 [a::INTEGER]\n",
        );
        let plan = Plan::parse(text).expect("an aggregate");
        let Node::Aggregate { aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is not an aggregate");
        };
        let call = plan.expr_list(aggregates)[0];
        assert!(matches!(plan.expr(call), Expr::Aggregate { .. }));
        assert!(!elementwise(&plan, call));
        assert!(!constant(&plan, call));
    }

    #[test]
    fn a_case_answers_for_the_whole_of_itself_and_not_for_the_branch_that_runs() {
        let both =
            filtered("CASE WHEN TRUE::BOOLEAN THEN TRUE::BOOLEAN ELSE FALSE::BOOLEAN END::BOOLEAN");
        assert!(constant(&both, predicate(&both)));
        // One branch reading a column is enough, because which branch runs is a per row answer.
        let one = filtered(
            "CASE WHEN TRUE::BOOLEAN THEN TRUE::BOOLEAN ELSE (#0.0::INTEGER > 1::INTEGER)::BOOLEAN END::BOOLEAN",
        );
        assert!(!constant(&one, predicate(&one)));
        assert!(elementwise(&one, predicate(&one)));
    }
}
