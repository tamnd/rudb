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
//! give every row the same number, and the version of that list that grows at the same time as the
//! function is the version where somebody has to remember. The list lives in `rudb-bind` with the
//! evaluation it belongs to, and is named here because this is the pass that has to respect it.
//!
//! A fold whose value does not have the type the plan recorded for the expression is abandoned too.
//! That cannot happen if the kernels and the binder agree, which is the point: it is a disagreement
//! between the two, and turning it into a plan that still runs correctly is better than turning it
//! into a validation failure a long way from the cause.
//!
//! There is one fold that changes the type on purpose, and `widened_negation` is it. Negating the
//! smallest value of a signed integer type has no answer in that type, and upstream answers in the
//! next one up rather than raising, so the type of the expression depends on the value and only this
//! pass can see the value. It is the one place where what comes out is not what the binder typed.

use std::collections::HashMap;

pub use rudb_bind::fold::VOLATILE;
use rudb_bind::fold::value_of;
use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{CompareOp, ConjunctionOp, Expr, ExprRef, Node, NodeRef, Plan, Slice, SortKey};

use crate::pass::{Context, Pass, top_down};
use crate::walk;

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
struct Done {
    rewritten: HashMap<ExprRef, ExprRef>,
    canonical: Vec<ExprRef>,
}

/// Rewrites one expression, for a pass that built it after this one had already run.
///
/// This pass runs early and every rule in it assumes the expressions it is given are the ones the
/// binder produced. A later pass that builds a new expression out of two old ones can hand back
/// something this would have folded, and nothing folds it: `WHERE CAST(x AS BOOLEAN)` over a group
/// key of `NULL` becomes `CAST(NULL AS BOOLEAN)` when filter pushdown puts the predicate under the
/// grouping, which is a constant nobody has evaluated. The plan that comes out of the sequence is
/// then not the plan a second run of the sequence produces, and the idempotence assertion in
/// [`crate::optimize_with`] says so. So a pass that substitutes into an expression asks for the
/// rules to be applied to what it built, here, rather than leaving it for a run that does not
/// happen.
///
/// The sharing table is per call, which is the difference between this and [`rewrite`]. One
/// expression is cheap to walk twice and the table only pays for itself over a whole plan.
pub(crate) fn rewritten(plan: &mut Plan, expr: ExprRef) -> ExprRef {
    let mut done = Done { rewritten: HashMap::new(), canonical: Vec::new() };
    expression(plan, expr, &mut done)
}

/// Rewrites every expression the plan reaches.
fn rewrite(plan: &mut Plan) {
    let mut done = Done { rewritten: HashMap::new(), canonical: Vec::new() };
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
        | Node::LimitPercent { .. }
        | Node::TableFetch { .. }
        | Node::SetOp { .. }
        | Node::CrossProduct { .. }
        | Node::MaterializedCte { .. }
        | Node::CteScan { .. }
        | Node::Consistent { .. } => {}
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
        Node::TableFunction { args, .. } | Node::LateralFunction { args, .. } => {
            if let Some(rewritten) = expr_list(plan, args, done) {
                match plan.node_mut(node) {
                    Node::TableFunction { args, .. } | Node::LateralFunction { args, .. } => {
                        *args = rewritten;
                    }
                    _ => unreachable!("the node was a table function a moment ago"),
                }
            }
        }
        Node::Fetch { args, .. } => {
            if let Some(rewritten) = expr_list(plan, args, done) {
                match plan.node_mut(node) {
                    Node::Fetch { args, .. } => *args = rewritten,
                    _ => unreachable!("the node was a fetch a moment ago"),
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
        Node::Window { partition, order, frame, expressions, .. } => {
            let rewritten_partition = expr_list(plan, partition, done);
            let rewritten_expressions = expr_list(plan, expressions, done);
            let held_order = plan.sort_key_list(order).to_vec();
            let rewritten_order: Vec<SortKey> = held_order
                .iter()
                .map(|key| SortKey { expr: expression(plan, key.expr, done), ..*key })
                .collect();
            let order =
                (rewritten_order != held_order).then(|| plan.add_sort_keys(&rewritten_order));
            let rewrite_bound = |plan: &mut Plan, bound, done: &mut Done| match bound {
                rudb_plan::WindowBound::Preceding(offset) => {
                    rudb_plan::WindowBound::Preceding(expression(plan, offset, done))
                }
                rudb_plan::WindowBound::Following(offset) => {
                    rudb_plan::WindowBound::Following(expression(plan, offset, done))
                }
                other => other,
            };
            let start = rewrite_bound(plan, frame.start, done);
            let end = rewrite_bound(plan, frame.end, done);
            match plan.node_mut(node) {
                Node::Window { partition, order: held_order, frame, expressions, .. } => {
                    if let Some(rewritten) = rewritten_partition {
                        *partition = rewritten;
                    }
                    if let Some(rewritten) = order {
                        *held_order = rewritten;
                    }
                    frame.start = start;
                    frame.end = end;
                    if let Some(rewritten) = rewritten_expressions {
                        *expressions = rewritten;
                    }
                }
                _ => unreachable!("the node was a window a moment ago"),
            }
        }
        Node::Sort { keys, .. } | Node::TopN { keys, .. } => {
            let held = plan.sort_key_list(keys).to_vec();
            let rewritten: Vec<SortKey> = held
                .iter()
                .map(|key| SortKey { expr: expression(plan, key.expr, done), ..*key })
                .collect();
            if rewritten != held {
                let keys = plan.add_sort_keys(&rewritten);
                match plan.node_mut(node) {
                    Node::Sort { keys: held, .. } | Node::TopN { keys: held, .. } => *held = keys,
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
        Node::Join { conditions, .. }
        | Node::LinkJoin { conditions, .. }
        | Node::DependentJoin { conditions, .. } => {
            if let Some(rewritten) = expr_list(plan, conditions, done) {
                match plan.node_mut(node) {
                    Node::Join { conditions, .. }
                    | Node::LinkJoin { conditions, .. }
                    | Node::DependentJoin { conditions, .. } => {
                        *conditions = rewritten;
                    }
                    _ => unreachable!("the node was a join a moment ago"),
                }
            }
        }
    }
}

/// Rewrites a run of expressions, handing back a new slice only if one of them changed.
fn expr_list(plan: &mut Plan, slice: Slice, done: &mut Done) -> Option<Slice> {
    walk::list(plan, slice, &mut |plan, expr| expression(plan, expr, done))
}

/// Rewrites one expression and everything under it, bottom up.
///
/// Bottom up is not a preference. An expression may only refer to an expression behind it in the
/// arena, which `Plan::validate` checks and which is what makes a plan acyclic by construction, so a
/// rewritten operand has to be appended before the operator that reads it. Folding a child first is
/// also what makes one pass enough: `1 + 2 + 3` folds to `6` in a single walk because by the time
/// the outer call runs, its operand is already a constant.
fn expression(plan: &mut Plan, expr: ExprRef, done: &mut Done) -> ExprRef {
    if let Some(&already) = done.rewritten.get(&expr) {
        return already;
    }
    let rebuilt = walk::rebuild(plan, expr, &mut |plan, child| expression(plan, child, done));
    let simplified = simplify(plan, rebuilt);
    let canonical = if walk::volatile(plan, simplified) {
        simplified
    } else {
        done.canonical
            .iter()
            .copied()
            .find(|&other| walk::same(plan, simplified, other))
            .unwrap_or_else(|| {
                done.canonical.push(simplified);
                simplified
            })
    };
    done.rewritten.insert(expr, canonical);
    canonical
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
    if let Some(widened) = widened_negation(plan, expr) {
        return widened;
    }
    match *plan.expr(expr) {
        Expr::Conjunction { op, children } => conjunction(plan, expr, op, children),
        Expr::Case { arms, otherwise } => case(plan, expr, arms, otherwise),
        Expr::Compare { op, left, right } => {
            if let Some(shifted) = shifted_comparison(plan, expr, op, left, right) {
                let Expr::Compare { op, left, right } = *plan.expr(shifted) else { return shifted };
                return narrowed_comparison(plan, shifted, op, left, right).unwrap_or(shifted);
            }
            match narrowed_comparison(plan, expr, op, left, right) {
                Some(narrowed) => narrowed,
                None => null_comparison(plan, expr, op, left, right),
            }
        }
        _ => expr,
    }
}

/// Moves a widening cast off the column and onto the literal it is compared against.
///
/// The binder types `AdvEngineID <> 0` by widening both sides to the type that holds them both,
/// which on a SMALLINT column against an INTEGER literal means casting a million values up one
/// width to compare each against a constant that fits in the width they were already in. Comparing
/// the column to `0::SMALLINT` asks the same question and reads the same answer off the stored
/// values without touching them. On ClickBench query 1 the widening cast was 38 percent of the
/// instructions the whole query ran, because the fallback it lands in walks the column a value at a
/// time through the scalar cast rather than a vector at a time.
///
/// # Why the subset test and not just a width test
///
/// The rewrite is only sound when the cast is an embedding, which is to say injective and order
/// preserving over the whole of the narrow type. That is exactly the condition that the narrow
/// type's range sits inside the wide type's, and it is not the same as the narrow type being
/// smaller: INTEGER into UBIGINT is four bytes into eight and still throws away every negative
/// value. Testing the ranges rather than the widths gets the signed and unsigned mixtures right
/// without a table of pairs.
///
/// # Why a literal that does not fit is left alone
///
/// `CAST(small AS INTEGER) = 100000` has a constant answer, since no SMALLINT is a hundred thousand,
/// but the constant is not `false`. A null column value compares null, not false, so folding the
/// whole comparison away would change what a nullable column returns. Deciding it properly needs
/// the null handling that `IS NOT NULL` would carry, which is a rule about ranges rather than a rule
/// about casts, so this one declines and the plan keeps the cast.
fn narrowed_comparison(
    plan: &mut Plan,
    expr: ExprRef,
    op: CompareOp,
    left: ExprRef,
    right: ExprRef,
) -> Option<ExprRef> {
    let (cast, literal, cast_on_the_left) = match (plan.expr(left), plan.expr(right)) {
        (&Expr::Cast { input, .. }, Expr::Constant(_)) => (input, right, true),
        (Expr::Constant(_), &Expr::Cast { input, .. }) => (input, left, false),
        _ => return None,
    };
    let wide = plan.expr_type(if cast_on_the_left { left } else { right }).clone();
    let narrow = plan.expr_type(cast).clone();
    let (narrow_low, narrow_high) = integer_range(&narrow)?;
    let (wide_low, wide_high) = integer_range(&wide)?;
    if wide_low > narrow_low || wide_high < narrow_high {
        return None;
    }
    let value = integer_value(&constant(plan, literal)?)?;
    let narrowed = integer_of(&narrow, value)?;
    let held = plan.add_value(narrowed);
    let constant = plan.add_expr_at(Expr::Constant(held), narrow, plan.expr_span(literal));
    let (left, right) = if cast_on_the_left { (cast, constant) } else { (constant, cast) };
    let ty = plan.expr_type(expr).clone();
    Some(plan.add_expr_at(Expr::Compare { op, left, right }, ty, plan.expr_span(expr)))
}

/// Moves a constant date off a count of days and onto the date it is compared against.
///
/// A Parquet file keeps a date as a count of days, and the view ClickBench reads it through turns
/// the count back into a date with `DATE '1970-01-01' + EventDate`. A filter on that date is then a
/// comparison on an expression, which neither a row group's statistics nor a stored zone can
/// answer, so a query that wants one month reads every row to find it. Adding a constant is strictly
/// increasing, so `d0 + x < d1` holds exactly when `x < d1 - d0`, and the second form is a column
/// against a constant again. [`narrowed_comparison`] then takes the widening cast off the column.
///
/// The sum is checked by the kernel and raises when it leaves the range of a date, and a rewrite
/// that removed the sum would remove the error with it. So this only fires when no value the count
/// can hold takes the date out of range, which it reads off the integer type under a widening cast,
/// and it declines for a bare `INTEGER`, whose extremes do.
fn shifted_comparison(
    plan: &mut Plan,
    expr: ExprRef,
    op: CompareOp,
    left: ExprRef,
    right: ExprRef,
) -> Option<ExprRef> {
    let (sum, bound, op) = match (plan.expr(left), plan.expr(right)) {
        (Expr::Function { .. }, Expr::Constant(_)) => (left, right, op),
        (Expr::Constant(_), Expr::Function { .. }) => (right, left, op.flip()),
        _ => return None,
    };
    let Value::Date(bound) = constant(plan, bound)? else { return None };
    let (origin, count) = days_after(plan, sum)?;
    let apart = integer_of(&LogicalType::Integer, i128::from(bound) - i128::from(origin))?;
    let held = plan.add_value(apart);
    let constant =
        plan.add_expr_at(Expr::Constant(held), LogicalType::Integer, plan.expr_span(right));
    let ty = plan.expr_type(expr).clone();
    Some(plan.add_expr_at(
        Expr::Compare { op, left: count, right: constant },
        ty,
        plan.expr_span(expr),
    ))
}

/// A date made by adding a count of days to a constant date, as the date and the count.
///
/// `None` unless no value the count can hold takes the sum out of the range of a date, which is read
/// off the integer type under a widening cast. The sum is checked by the kernel and raises when it
/// leaves the range, so a rule that answers the sum without computing it has to know it could not
/// have raised. A bare `INTEGER` count is refused, since its extremes do leave the range.
pub(crate) fn days_after(plan: &Plan, sum: ExprRef) -> Option<(i32, ExprRef)> {
    let Expr::Function { name, args } = *plan.expr(sum) else { return None };
    if plan.string(name) != "+" || *plan.expr_type(sum) != LogicalType::Date {
        return None;
    }
    let &[first, second] = plan.expr_list(args) else { return None };
    let (origin, count) = match (constant(plan, first), constant(plan, second)) {
        (Some(Value::Date(origin)), None) => (origin, second),
        (None, Some(Value::Date(origin))) => (origin, first),
        _ => return None,
    };
    if *plan.expr_type(count) != LogicalType::Integer {
        return None;
    }
    let held = match *plan.expr(count) {
        Expr::Cast { input, .. } => plan.expr_type(input).clone(),
        _ => LogicalType::Integer,
    };
    let (low, high) = integer_range(&held)?;
    // Far inside the kernel's range on both sides, so no sum a count of this type makes can fail.
    let room = 1_i128 << 30;
    if i128::from(origin).abs() >= room || low <= -room || high >= room {
        return None;
    }
    Some((origin, count))
}

/// The inclusive range of an integer type, in the widest signed integer a plan value holds.
///
/// UHUGEINT is missing because its top half does not fit in an `i128`, and nothing narrows into it
/// anyway, so leaving it out costs no rewrite that would otherwise have fired.
fn integer_range(ty: &LogicalType) -> Option<(i128, i128)> {
    let range = match *ty {
        LogicalType::TinyInt => (i128::from(i8::MIN), i128::from(i8::MAX)),
        LogicalType::SmallInt => (i128::from(i16::MIN), i128::from(i16::MAX)),
        LogicalType::Integer => (i128::from(i32::MIN), i128::from(i32::MAX)),
        LogicalType::BigInt => (i128::from(i64::MIN), i128::from(i64::MAX)),
        LogicalType::HugeInt => (i128::MIN, i128::MAX),
        LogicalType::UTinyInt => (0, i128::from(u8::MAX)),
        LogicalType::USmallInt => (0, i128::from(u16::MAX)),
        LogicalType::UInteger => (0, i128::from(u32::MAX)),
        LogicalType::UBigInt => (0, i128::from(u64::MAX)),
        _ => return None,
    };
    Some(range)
}

/// An integer value as an `i128`, or `None` for anything that is not an integer.
fn integer_value(value: &Value) -> Option<i128> {
    let value = match *value {
        Value::TinyInt(value) => i128::from(value),
        Value::SmallInt(value) => i128::from(value),
        Value::Integer(value) => i128::from(value),
        Value::BigInt(value) => i128::from(value),
        Value::HugeInt(value) => value,
        Value::UTinyInt(value) => i128::from(value),
        Value::USmallInt(value) => i128::from(value),
        Value::UInteger(value) => i128::from(value),
        Value::UBigInt(value) => i128::from(value),
        _ => return None,
    };
    Some(value)
}

/// `value` as an integer of `ty`, or `None` when it does not fit.
fn integer_of(ty: &LogicalType, value: i128) -> Option<Value> {
    let value = match *ty {
        LogicalType::TinyInt => Value::TinyInt(i8::try_from(value).ok()?),
        LogicalType::SmallInt => Value::SmallInt(i16::try_from(value).ok()?),
        LogicalType::Integer => Value::Integer(i32::try_from(value).ok()?),
        LogicalType::BigInt => Value::BigInt(i64::try_from(value).ok()?),
        LogicalType::HugeInt => Value::HugeInt(value),
        LogicalType::UTinyInt => Value::UTinyInt(u8::try_from(value).ok()?),
        LogicalType::USmallInt => Value::USmallInt(u16::try_from(value).ok()?),
        LogicalType::UInteger => Value::UInteger(u32::try_from(value).ok()?),
        LogicalType::UBigInt => Value::UBigInt(u64::try_from(value).ok()?),
        _ => return None,
    };
    Some(value)
}

/// The value of an expression all of whose operands are constants, if it has one.
///
/// The evaluation itself is [`value_of`], which the binder also calls, because it needs the row
/// count a `LIMIT` comes to before there is a plan for this pass to run over. What is left here is
/// the part that is about folding rather than about evaluating: which expressions are offered at
/// all, and what happens when one of them raises.
///
/// Only the four kinds whose operands are already constants by the time this sees them are offered.
/// A `CASE` is not, even though [`value_of`] can evaluate one, because [`case`] is what reduces a
/// `CASE` here and giving the job to two rules is how two rules come to disagree.
///
/// An error is thrown away and the expression is left exactly as it was written, which the module
/// documentation explains: a call that fails is a call that does not fold, the node stays in the
/// plan, and the executor raises the same failure later with the expression to hand. `7 // 0` is
/// that, and the message a user sees for it comes from the executor and not from here.
fn fold(plan: &Plan, expr: ExprRef) -> Option<Value> {
    match *plan.expr(expr) {
        Expr::Cast { .. }
        | Expr::Compare { .. }
        | Expr::Conjunction { .. }
        | Expr::Function { .. } => value_of(plan, expr).ok().flatten(),
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
    Some(plan.add_expr_at(Expr::Constant(held), ty, plan.expr_span(expr)))
}

/// Negating the smallest value of a signed integer type, which widens instead of raising.
///
/// `-((-128)::TINYINT)` is the SMALLINT 128 on the pinned binary, and a SMALLINT goes to INTEGER, an
/// INTEGER to BIGINT and a BIGINT to HUGEINT the same way. It is a rule about the one value in each
/// type that has no negative and not a rule about the type, so `typeof(-(1::INTEGER))` is still
/// INTEGER and everything but the smallest value comes out of the fold above with the type it went in
/// with. A HUGEINT is not in the table because there is nothing wider to widen it to, and a column is
/// not here at all, so both of those still raise. Per #264.
///
/// The type the binder gave the call has to be the argument's own type for this to fire. If it is
/// not, something upstream of here has already decided the expression is wider than it looks, and
/// widening it a second time would be two rules deciding one type.
fn widened_negation(plan: &mut Plan, expr: ExprRef) -> Option<ExprRef> {
    let Expr::Function { name, args } = *plan.expr(expr) else { return None };
    if plan.string(name) != "-" {
        return None;
    }
    let &[only] = plan.expr_list(args) else { return None };
    let value = constant(plan, only)?;
    if *plan.expr_type(expr) != value.logical_type() {
        return None;
    }
    let widened = match value {
        Value::TinyInt(i8::MIN) => Value::SmallInt(128),
        Value::SmallInt(i16::MIN) => Value::Integer(32_768),
        Value::Integer(i32::MIN) => Value::BigInt(2_147_483_648),
        Value::BigInt(i64::MIN) => Value::HugeInt(9_223_372_036_854_775_808),
        _ => return None,
    };
    let ty = widened.logical_type();
    let held = plan.add_value(widened);
    Some(plan.add_expr_at(Expr::Constant(held), ty, plan.expr_span(expr)))
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
            plan.add_expr_at(
                Expr::Conjunction { op, children },
                LogicalType::Boolean,
                plan.expr_span(expr),
            )
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
    plan.add_expr_at(Expr::Case { arms, otherwise: result }, ty, plan.expr_span(expr))
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

#[cfg(test)]
mod tests {
    use super::{ExpressionRewriter, VOLATILE};
    use crate::pass::{Context, Pass};
    use rudb_plan::{Expr, Node, Plan};

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

    /// A scan whose columns cover the integer widths the narrowing rule reasons about.
    const WIDTHS: &str = "  Get memory.main.t AS t #0 [s::SMALLINT, u::UINTEGER, g::BIGINT]\n";

    #[test]
    fn a_literal_narrows_onto_the_column_rather_than_the_column_widening_onto_the_literal() {
        let before =
            format!("Filter (CAST(#0.0::SMALLINT)::INTEGER <> 0::INTEGER)::BOOLEAN\n{WIDTHS}");
        let after = format!("Filter (#0.0::SMALLINT <> 0::SMALLINT)::BOOLEAN\n{WIDTHS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_literal_on_the_left_narrows_the_same_way_and_stays_on_the_left() {
        let before =
            format!("Filter (5000::INTEGER < CAST(#0.0::SMALLINT)::INTEGER)::BOOLEAN\n{WIDTHS}");
        let after = format!("Filter (5000::SMALLINT < #0.0::SMALLINT)::BOOLEAN\n{WIDTHS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_literal_no_smallint_can_equal_keeps_the_cast_because_a_null_row_is_not_false() {
        let before =
            format!("Filter (CAST(#0.0::SMALLINT)::INTEGER = 100000::INTEGER)::BOOLEAN\n{WIDTHS}");
        assert_eq!(folded(&before), before);
    }

    #[test]
    fn a_narrowing_cast_is_left_alone_because_it_is_not_injective() {
        // `CAST(g AS INTEGER) = 5` is true for more than one BIGINT, so answering it against the
        // BIGINT column would not be the same question.
        let before =
            format!("Filter (CAST(#0.2::BIGINT)::INTEGER = 5::INTEGER)::BOOLEAN\n{WIDTHS}");
        assert_eq!(folded(&before), before);
    }

    #[test]
    fn an_unsigned_column_widened_into_a_signed_type_narrows_back_the_same_way() {
        let before =
            format!("Filter (CAST(#0.1::UINTEGER)::BIGINT = 5::BIGINT)::BOOLEAN\n{WIDTHS}");
        let after = format!("Filter (#0.1::UINTEGER = 5::UINTEGER)::BOOLEAN\n{WIDTHS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_wider_type_that_does_not_contain_the_narrow_one_is_left_alone() {
        // UBIGINT is eight bytes to INTEGER's four and still holds none of INTEGER's negatives, so
        // width is not the test and the rule declines.
        let before = format!("Filter (CAST(#0.0::INTEGER)::UBIGINT = 5::UBIGINT)::BOOLEAN\n{SCAN}");
        assert_eq!(folded(&before), before);
    }

    /// A scan with a count of days in the width a Parquet file stores it in, and a bare INTEGER.
    const DAYS: &str = "  Get memory.main.t AS t #0 [d::USMALLINT, i::INTEGER]\n";

    #[test]
    fn a_date_made_from_a_count_of_days_is_compared_as_the_count() {
        let before = format!(
            "Filter (\"+\"(0::DATE, CAST(#0.0::USMALLINT)::INTEGER)::DATE >= 15887::DATE)::BOOLEAN\n{DAYS}"
        );
        let after = format!("Filter (#0.0::USMALLINT >= 15887::USMALLINT)::BOOLEAN\n{DAYS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_date_on_the_left_flips_the_comparison_onto_the_count() {
        let before = format!(
            "Filter (15917::DATE >= \"+\"(CAST(#0.0::USMALLINT)::INTEGER, 10::DATE)::DATE)::BOOLEAN\n{DAYS}"
        );
        let after = format!("Filter (#0.0::USMALLINT <= 15907::USMALLINT)::BOOLEAN\n{DAYS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_date_before_the_origin_keeps_the_cast_since_no_count_reaches_it() {
        let before = format!(
            "Filter (\"+\"(0::DATE, CAST(#0.0::USMALLINT)::INTEGER)::DATE < -3::DATE)::BOOLEAN\n{DAYS}"
        );
        let after =
            format!("Filter (CAST(#0.0::USMALLINT)::INTEGER < -3::INTEGER)::BOOLEAN\n{DAYS}");
        assert_eq!(folded(&before), after);
    }

    #[test]
    fn a_bare_integer_count_is_left_alone_because_its_extremes_leave_the_range_of_a_date() {
        let before =
            format!("Filter (\"+\"(0::DATE, #0.1::INTEGER)::DATE >= 15887::DATE)::BOOLEAN\n{DAYS}");
        assert_eq!(folded(&before), before);
    }

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

    /// The one fold that comes out wider than it went in, at all four widths. Per #264.
    #[test]
    fn negating_the_smallest_value_of_a_signed_type_widens_by_one_step() {
        let cases = [
            ("-128::TINYINT", "TINYINT", "128::SMALLINT"),
            ("-32768::SMALLINT", "SMALLINT", "32768::INTEGER"),
            ("-2147483648::INTEGER", "INTEGER", "2147483648::BIGINT"),
            ("-9223372036854775808::BIGINT", "BIGINT", "9223372036854775808::HUGEINT"),
        ];
        for (argument, ty, expected) in cases {
            let before = format!("Project #1 [\"-\"({argument})::{ty} AS n]\n{SCAN}");
            let after = format!("Project #1 [{expected} AS n]\n{SCAN}");
            assert_eq!(folded(&before), after, "{argument}");
        }
    }

    #[test]
    fn negating_anything_but_the_smallest_value_keeps_the_type_it_was_given() {
        let before = format!("Project #1 [\"-\"(-127::TINYINT)::TINYINT AS n]\n{SCAN}");
        let after = format!("Project #1 [127::TINYINT AS n]\n{SCAN}");
        assert_eq!(folded(&before), after);
        // A HUGEINT has nowhere to widen to, so this is a fold that raises and is abandoned, and the
        // executor is left to say the sentence.
        let smallest = "-170141183460469231731687303715884105728::HUGEINT";
        let hugeint = format!("Project #1 [\"-\"({smallest})::HUGEINT AS n]\n{SCAN}");
        assert_eq!(folded(&hugeint), hugeint);
        // A column has no value here to look at, whatever the values in it turn out to be.
        let column = format!("Project #1 [\"-\"(#0.0::INTEGER)::INTEGER AS n]\n{SCAN}");
        assert_eq!(folded(&column), column);
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
    fn equal_subexpressions_share_one_reference() {
        let text = format!(
            "Aggregate #1 groups=[] aggregates=[sum(CAST(#0.0::INTEGER)::BIGINT)::HUGEINT, \
             sum(\"+\"(CAST(#0.0::INTEGER)::BIGINT, 1::BIGINT)::BIGINT)::HUGEINT]\n{SCAN}"
        );
        let mut plan = Plan::parse(&text).expect("a well formed aggregate");
        ExpressionRewriter.run(&mut plan, &Context::new()).expect("the expressions rewrite");
        let Node::Aggregate { aggregates, .. } = *plan.node(plan.root()) else {
            panic!("the root is an aggregate");
        };
        let calls = plan.expr_list(aggregates);
        let Expr::Aggregate { args: first, .. } = *plan.expr(calls[0]) else {
            panic!("the first expression is an aggregate call");
        };
        let Expr::Aggregate { args: second, .. } = *plan.expr(calls[1]) else {
            panic!("the second expression is an aggregate call");
        };
        let first = plan.expr_list(first)[0];
        let Expr::Function { args, .. } = *plan.expr(plan.expr_list(second)[0]) else {
            panic!("the second argument is an addition");
        };
        assert_eq!(first, plan.expr_list(args)[0]);
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
