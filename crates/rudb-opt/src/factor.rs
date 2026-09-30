//! Summing a product once for each value of its small factor, rather than once for each row.
//!
//! TPC-H q01 asks for `sum(l_extendedprice * (1 - l_discount))` and the same times `(1 + l_tax)`
//! over six million rows in four groups. Each row pays for two decimal products and adds three
//! totals, when `l_discount` takes eleven values and `l_tax` nine. Grouped by the two of them as
//! well as by the flag and the status, the rows only add up `l_extendedprice`, and the products are
//! taken once per group of the result below, which is a few hundred rows. The sum of `p * (1 - d)`
//! over the rows of one discount is `(1 - d)` times the sum of `p` over them, so the grouping above
//! adds up the same numbers.
//!
//! # When it is the same answer
//!
//! A `sum` over integers or decimals whose argument is a product, where some of the factors read
//! only columns with few values and the one factor left reads the rest. Grouping by those columns
//! as well as the keys, the partial sum is of the one factor, and the sum above is of the small
//! factors times that partial. Integers and decimals multiply and add exactly, so the two are the
//! same number. A null in a small factor is a group of its own below and a null product above,
//! which the sum skips the way it skipped each row, and a group whose partial is null because every
//! row of it was null is skipped the same way.
//!
//! The one difference is overflow. A product per row that did not fit its type raised an error,
//! and the rewrite multiplies a total instead, in the type the product of the two has, which is
//! wider. So a query that overflowed on one row can answer here, and never the other way round.
//!
//! The other calls come along. A `sum` with no small factor is a sum of partial sums. A `sum` of
//! small factors only is their product times a count of the rows. A `count` and a `count(*)` are
//! sums of partial counts, declared `BIGINT`. A `min` or a `max` is the same call again. An `avg` of
//! a column is its partial sum and count put back together by `__rudb_mean`, which is the division
//! `avg` itself does, so the bits are the same, and an `avg` of a small column is that column times
//! the partial count. Both are held to integers of at most 64 bits and decimals of at most eighteen
//! digits, as [`crate::shared`] holds them, since that is where the exact total cannot overflow. A
//! `DISTINCT`, a `FILTER`, a float and any other call leave the aggregate as it is.
//!
//! # When it is worth it
//!
//! Every small column has at most [`SMALL`] values, as the file says, and the keys and the small
//! columns together have at most one group for each [`SHRINK`] rows under the aggregate. A decimal
//! column's count is the span between its two ends, which the native file states for every column
//! it wrote exact ends for. At least one `sum` has a small factor beside a factor that is not, since
//! that is the product the rows stop paying for, and the grouping below adds up fewer sums per row
//! than the aggregate did, since it also pays for one more key per row. q01's five sums and averages
//! become two sums below, and q06's `sum(l_extendedprice * l_discount)` would stay one, so q06 is
//! left as it is.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #1 groups=[#0.0] aggregates=[sum("*"(#0.1, "-"(1, #0.2))), avg(#0.2), count_star()]
//! ```
//!
//! becomes
//!
//! ```text
//! Project #1 [#4.0, #4.1, __rudb_mean(#4.2, #4.3), #4.4]
//!   Aggregate #4 groups=[#3.0] aggregates=[sum("*"("-"(1, #3.1), #3.2)), sum("*"(#3.1, #3.3)),
//!                                          sum(#3.3), sum(#3.4)]
//!     Aggregate #3 groups=[#0.0, #0.2] aggregates=[sum(#0.1), count(#0.2), count_star()]
//! ```
//!
//! The projection is only there when an `avg` is. The grouping below keeps whatever is under the
//! aggregate, a filter included, and a second run finds the top grouping reading a grouping and
//! leaves it.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result, Span};
use rudb_functions::signature;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

use crate::eager::rebind;
use crate::estimate::{self, Facts};
use crate::fromkey;
use crate::pass::{Context, Pass};
use crate::walk;

/// Groups by the small factors of a sum's products before the grouping asks for them.
#[derive(Debug, Clone, Copy)]
pub struct Factoring;

impl Pass for Factoring {
    fn name(&self) -> &'static str {
        "factoring"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        factor(plan, context.facts());
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that this applies to.
pub fn factor(plan: &mut Plan, stats: &Facts) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut |plan, at| split(plan, at, stats));
    if moved {
        plan.set_root(root);
    }
}

/// The most values a column can take and still be a small factor.
const SMALL: u64 = 32;

/// How many rows under the aggregate each group of the grouping below has to stand for.
const SHRINK: u64 = 64;

/// A total the grouping above adds up: a partial, times the small factors when there are any.
#[derive(Clone, Copy)]
struct Total {
    /// The product of the small factors, over the columns below, or `None` for the partial alone.
    times: Option<ExprRef>,
    /// Which partial.
    partial: usize,
}

/// What one call of the aggregate becomes.
#[derive(Clone, Copy)]
enum Split {
    /// A sum of the total, typed as the call was.
    Sum(Total),
    /// A sum of a partial count, declared `BIGINT`.
    Counted(usize),
    /// The same call again over a partial.
    Again(usize),
    /// `__rudb_mean` of a total and a partial count.
    Mean(Total, usize),
}

/// How the factors of one `sum` fall.
struct Factors {
    /// The factors that read only small columns and keys, or nothing at all.
    small: Vec<ExprRef>,
    /// The factors that read anything else.
    rest: Vec<ExprRef>,
}

/// The two stage form of `at` when it is an aggregate this applies to.
fn split(plan: &mut Plan, at: NodeRef, stats: &Facts) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let below = match *plan.node(input) {
        Node::Project { input, .. } | Node::Filter { input, .. } => input,
        _ => input,
    };
    if matches!(plan.node(input), Node::Aggregate { .. })
        || matches!(plan.node(below), Node::Aggregate { .. })
    {
        return None;
    }
    let keys = plan.expr_list(groups).to_vec();
    let calls = plan.expr_list(aggregates).to_vec();
    let &rows = estimate::unfiltered(plan, input, stats).value()?;

    // The keys that are a column, which a small factor may read as well, since each group below
    // holds one value of each. How many groups they make is the product of what each can take.
    let mut keyed = Vec::new();
    let mut groups_below: u64 = 1;
    for &key in &keys {
        let Expr::Column(binding) = *plan.expr(key) else { return None };
        let &distinct = estimate::stated(plan, binding, stats).value()?;
        groups_below = groups_below.checked_mul(distinct.max(1))?;
        keyed.push(binding);
    }
    let small = |plan: &Plan, binding: ColumnBinding| {
        keyed.contains(&binding)
            || estimate::stated(plan, binding, stats).value().is_some_and(|&n| n <= SMALL)
    };

    // The columns the small factors read that are not keys, which become keys below.
    let mut factored: Vec<ColumnBinding> = Vec::new();
    let mut sums: Vec<Option<Factors>> = Vec::with_capacity(calls.len());
    for &call in &calls {
        sums.push(None);
        let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else {
            return None;
        };
        if distinct || filter.is_some() {
            return None;
        }
        let args = plan.expr_list(args).to_vec();
        if plan.string(name) != "sum" {
            continue;
        }
        let [arg] = args[..] else { return None };
        if !exact(plan.expr_type(arg)) {
            return None;
        }
        let mut factors = Vec::new();
        flatten(plan, arg, &mut factors);
        let mut sorted = Factors { small: Vec::new(), rest: Vec::new() };
        for factor in factors {
            let mut read = Vec::new();
            walk::columns(plan, factor, &mut |binding| read.push(binding));
            let fits = walk::elementwise(plan, factor)
                && !walk::volatile(plan, factor)
                && read.iter().all(|&binding| small(plan, binding));
            if fits { sorted.small.push(factor) } else { sorted.rest.push(factor) }
        }
        if sorted.rest.len() == 1 {
            for &factor in &sorted.small {
                walk::columns(plan, factor, &mut |binding| {
                    if !keyed.contains(&binding) && !factored.contains(&binding) {
                        factored.push(binding);
                    }
                });
            }
        }
        *sums.last_mut()? = Some(sorted);
    }
    if factored.is_empty() {
        return None;
    }
    for &binding in &factored {
        let &distinct = estimate::stated(plan, binding, stats).value()?;
        groups_below = groups_below.checked_mul(distinct.max(1))?;
    }
    if groups_below.checked_mul(SHRINK)? > rows {
        return None;
    }
    let types: HashMap<ColumnBinding, LogicalType> =
        walk::outputs(plan, input)?.into_iter().collect();

    let mut partials: Vec<ExprRef> = Vec::new();
    let mut splits = Vec::with_capacity(calls.len());
    for (&call, factors) in calls.iter().zip(sums) {
        splits.push(partial(plan, call, factors, &keyed, &factored, &mut partials)?);
    }
    // The grouping below costs a key per row of its own, so it has to add up fewer totals per row
    // than the aggregate did. `sum(p * d)` alone becomes `sum(p)` by `d`, which is no fewer.
    let added = |name: &str| name == "sum" || name == "avg";
    let before =
        calls.iter().filter(|&&call| aggregate_name(plan, call).is_some_and(added)).count();
    let after = partials
        .iter()
        .filter(|&&call| aggregate_name(plan, call).is_some_and(|n| n == "sum"))
        .count();
    if after >= before {
        return None;
    }
    rewrite(plan, input, index, &keys, &factored, &types, &calls, &partials, &splits)
}

/// The name of the aggregate call `expr` is.
fn aggregate_name(plan: &Plan, expr: ExprRef) -> Option<&str> {
    match *plan.expr(expr) {
        Expr::Aggregate { name, .. } => Some(plan.string(name)),
        _ => None,
    }
}

/// Whether a sum over `ty` adds exactly, which is integers and decimals.
fn exact(ty: &LogicalType) -> bool {
    ty.is_integer() || matches!(ty, LogicalType::Decimal { .. })
}

/// Whether `ty` is a type whose exact total cannot overflow, as [`crate::shared`] asks of an `avg`.
fn narrow(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Decimal { width, .. } => *width <= 18,
        LogicalType::HugeInt | LogicalType::UHugeInt => false,
        other => other.is_integer(),
    }
}

/// The factors of a product, a nested `*` taken apart.
fn flatten(plan: &Plan, expr: ExprRef, into: &mut Vec<ExprRef>) {
    if let Expr::Function { name, args } = *plan.expr(expr)
        && plan.string(name) == "*"
        && let [left, right] = *plan.expr_list(args)
    {
        flatten(plan, left, into);
        flatten(plan, right, into);
        return;
    }
    into.push(expr);
}

/// `expr` without the casts on top of it that keep its value, which is a decimal or an integer
/// widened at the same scale. `sum(p)` and `sum(CAST(p AS DECIMAL(18,2)))` are the same total and
/// the binder writes the second inside a product, so taking the cast off lets the two share one.
fn uncast(plan: &Plan, mut expr: ExprRef) -> ExprRef {
    while let Expr::Cast { input, try_cast: false } = *plan.expr(expr) {
        let keeps = match (plan.expr_type(input), plan.expr_type(expr)) {
            (
                LogicalType::Decimal { width: one, scale: held },
                LogicalType::Decimal { width: other, scale: into },
            ) => held == into && one <= other,
            (from, to) => {
                signed_bits(from).zip(signed_bits(to)).is_some_and(|(one, other)| one <= other)
            }
        };
        if !keeps {
            break;
        }
        expr = input;
    }
    expr
}

/// How wide a signed integer type is, and `None` for any other type.
fn signed_bits(ty: &LogicalType) -> Option<u32> {
    match ty {
        LogicalType::TinyInt => Some(8),
        LogicalType::SmallInt => Some(16),
        LogicalType::Integer => Some(32),
        LogicalType::BigInt => Some(64),
        _ => None,
    }
}

/// The partial `call` names, added to `partials` unless one the same is there already.
fn keep(
    plan: &mut Plan,
    name: &str,
    arg: Option<ExprRef>,
    span: Span,
    partials: &mut Vec<ExprRef>,
) -> Option<usize> {
    let (args, ty) = match arg {
        Some(arg) => {
            let ty = if name == "count" {
                LogicalType::BigInt
            } else {
                let arg_ty = plan.expr_type(arg).clone();
                signature::resolve(name, &[arg_ty]).ok()?.returns
            };
            (vec![arg], ty)
        }
        None => (Vec::new(), LogicalType::BigInt),
    };
    for (at, &held) in partials.iter().enumerate() {
        let Expr::Aggregate { name: held_name, args: held_args, .. } = *plan.expr(held) else {
            continue;
        };
        let held_args = plan.expr_list(held_args).to_vec();
        if plan.string(held_name) == name
            && held_args.len() == args.len()
            && held_args.iter().zip(&args).all(|(&one, &other)| walk::same(plan, one, other))
        {
            return Some(at);
        }
    }
    let list = plan.add_expr_list(&args);
    let call =
        Expr::Aggregate { name: plan.intern(name), args: list, distinct: false, filter: None };
    partials.push(plan.add_expr_at(call, ty, span));
    Some(partials.len() - 1)
}

/// The product of `factors` under the binder's rule for `*`, or `None` for none.
fn product(plan: &mut Plan, factors: &[ExprRef], span: Span) -> Option<Option<ExprRef>> {
    let mut held: Option<ExprRef> = None;
    for &factor in factors {
        held = Some(match held {
            None => factor,
            Some(left) => apply(plan, "*", left, factor, span)?,
        });
    }
    Some(held)
}

/// `name(left, right)` with the casts the binder would put on each side.
fn apply(
    plan: &mut Plan,
    name: &str,
    left: ExprRef,
    right: ExprRef,
    span: Span,
) -> Option<ExprRef> {
    let types = [plan.expr_type(left).clone(), plan.expr_type(right).clone()];
    let resolved = signature::resolve(name, &types).ok()?;
    let [into_left, into_right] = &resolved.arguments[..] else { return None };
    let left = fromkey::cast(plan, left, into_left, span);
    let right = fromkey::cast(plan, right, into_right, span);
    let args = plan.add_expr_list(&[left, right]);
    let name = plan.intern(name);
    Some(plan.add_expr_at(Expr::Function { name, args }, resolved.returns, span))
}

/// Adds what `call` needs from the grouping below to `partials`, and says how the call is answered
/// from them. Nothing when the call cannot be done in two stages.
fn partial(
    plan: &mut Plan,
    call: ExprRef,
    factors: Option<Factors>,
    keyed: &[ColumnBinding],
    factored: &[ColumnBinding],
    partials: &mut Vec<ExprRef>,
) -> Option<Split> {
    let Expr::Aggregate { name, args, .. } = *plan.expr(call) else { return None };
    let args = plan.expr_list(args).to_vec();
    let returns = plan.expr_type(call).clone();
    let span = plan.expr_span(call);
    let name = plan.string(name).to_owned();
    let reads_small = |plan: &Plan, expr: ExprRef| {
        let mut all = true;
        walk::columns(plan, expr, &mut |binding| {
            all &= keyed.contains(&binding) || factored.contains(&binding);
        });
        all
    };
    match (name.as_str(), args.as_slice()) {
        ("count_star", []) if returns == LogicalType::BigInt => {
            Some(Split::Counted(keep(plan, "count_star", None, span, partials)?))
        }
        ("count", [arg]) if returns == LogicalType::BigInt => {
            Some(Split::Counted(keep(plan, "count", Some(*arg), span, partials)?))
        }
        ("min" | "max", [arg]) => {
            Some(Split::Again(keep(plan, &name, Some(*arg), span, partials)?))
        }
        ("sum", [arg]) => {
            let Factors { small, rest } = factors?;
            // Small factors that all read keys of the grouping below come out of the rows. Any
            // other sum is added up below as it is and summed again above.
            let fits = small.iter().all(|&factor| reads_small(plan, factor));
            let total = match rest.as_slice() {
                [] if fits => {
                    let times = product(plan, &small, span)??;
                    let partial = keep(plan, "count_star", None, span, partials)?;
                    Total { times: Some(widened(plan, times, span)), partial }
                }
                [only] if fits => {
                    let only = uncast(plan, *only);
                    let partial = keep(plan, "sum", Some(only), span, partials)?;
                    Total { times: product(plan, &small, span)?, partial }
                }
                _ => {
                    let whole = uncast(plan, *arg);
                    Total { times: None, partial: keep(plan, "sum", Some(whole), span, partials)? }
                }
            };
            Some(Split::Sum(total))
        }
        ("avg", [arg]) if returns == LogicalType::Double => {
            if !narrow(plan.expr_type(*arg)) {
                return None;
            }
            let count = keep(plan, "count", Some(*arg), span, partials)?;
            let total = if reads_small(plan, *arg) {
                Total { times: Some(widened(plan, *arg, span)), partial: count }
            } else {
                Total { times: None, partial: keep(plan, "sum", Some(*arg), span, partials)? }
            };
            Some(Split::Mean(total, count))
        }
        _ => None,
    }
}

/// `expr` cast to `HUGEINT` when it is an integer, so that a product of it and a count cannot
/// overflow the way a `BIGINT` product could. A decimal widens itself when it multiplies.
fn widened(plan: &mut Plan, expr: ExprRef, span: Span) -> ExprRef {
    if plan.expr_type(expr).is_integer() {
        fromkey::cast(plan, expr, &LogicalType::HugeInt, span)
    } else {
        expr
    }
}

/// Puts the grouping by the keys and the small columns over `input` and the grouping by the keys
/// over that.
#[expect(clippy::too_many_arguments, reason = "the pieces of one aggregate, taken apart")]
fn rewrite(
    plan: &mut Plan,
    input: NodeRef,
    index: u32,
    keys: &[ExprRef],
    factored: &[ColumnBinding],
    types: &HashMap<ColumnBinding, LogicalType>,
    calls: &[ExprRef],
    partials: &[ExprRef],
    splits: &[Split],
) -> Option<NodeRef> {
    let below = walk::fresh_index(plan);
    let mut inner_keys = keys.to_vec();
    for binding in factored {
        let ty = types.get(binding)?.clone();
        inner_keys.push(plan.add_expr(Expr::Column(*binding), ty));
    }
    let width = inner_keys.len();
    let read = |plan: &mut Plan, at: usize, ty: LogicalType| {
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(below, at)), ty)
    };
    // Every column the keys and the small factors read, pointed at where the grouping below puts it.
    let mut moved = HashMap::new();
    for (at, &key) in inner_keys.iter().enumerate() {
        if let Expr::Column(binding) = *plan.expr(key) {
            moved.insert(binding, ColumnBinding::new(below, u32::try_from(at).ok()?));
        }
    }
    let mut outer_keys = Vec::with_capacity(keys.len());
    for (at, &key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key).clone();
        outer_keys.push(read(plan, at, ty));
    }

    let mean = splits.iter().any(|split| matches!(split, Split::Mean(..)));
    let above = if mean { below + 1 } else { index };
    let mut outer_calls = Vec::new();
    let total = |plan: &mut Plan, name: &str, arg: ExprRef, ty: LogicalType, span| {
        let args = plan.add_expr_list(&[arg]);
        let call = Expr::Aggregate { name: plan.intern(name), args, distinct: false, filter: None };
        plan.add_expr_at(call, ty, span)
    };
    // The argument of the sum above for a total, and the type that sum answers in.
    let summed = |plan: &mut Plan, of: Total, span| -> Option<(ExprRef, LogicalType)> {
        let partial_ty = plan.expr_type(partials[of.partial]).clone();
        let partial = read(plan, width + of.partial, partial_ty);
        let arg = match of.times {
            None => partial,
            Some(times) => {
                let times = rebind(plan, times, &moved);
                apply(plan, "*", times, partial, span)?
            }
        };
        let ty = signature::resolve("sum", &[plan.expr_type(arg).clone()]).ok()?.returns;
        Some((arg, ty))
    };
    // Where each call's answer is among the outer aggregates: one of them, or a total and a count.
    let mut answers: Vec<(usize, Option<usize>)> = Vec::with_capacity(calls.len());
    for (&call, &split) in calls.iter().zip(splits) {
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let Expr::Aggregate { name, .. } = *plan.expr(call) else { return None };
        let name = plan.string(name).to_owned();
        match split {
            Split::Sum(of) => {
                let (arg, summed_ty) = summed(plan, of, span)?;
                if summed_ty != ty {
                    return None;
                }
                outer_calls.push(total(plan, "sum", arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Counted(at) => {
                let arg = read(plan, width + at, LogicalType::BigInt);
                outer_calls.push(total(plan, "sum", arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Again(at) => {
                let partial_ty = plan.expr_type(partials[at]).clone();
                let arg = read(plan, width + at, partial_ty);
                outer_calls.push(total(plan, &name, arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Mean(of, count) => {
                let (arg, summed_ty) = summed(plan, of, span)?;
                if !matches!(summed_ty, LogicalType::Decimal { .. } | LogicalType::HugeInt) {
                    return None;
                }
                outer_calls.push(total(plan, "sum", arg, summed_ty, span));
                let arg = read(plan, width + count, LogicalType::BigInt);
                outer_calls.push(total(plan, "sum", arg, LogicalType::BigInt, span));
                answers.push((outer_calls.len() - 2, Some(outer_calls.len() - 1)));
            }
        }
    }

    let groups = plan.add_expr_list(&inner_keys);
    let aggregates = plan.add_expr_list(partials);
    let inner = plan.add_node(Node::Aggregate { input, index: below, groups, aggregates });
    let groups = plan.add_expr_list(&outer_keys);
    let aggregates = plan.add_expr_list(&outer_calls);
    let outer = plan.add_node(Node::Aggregate { input: inner, index: above, groups, aggregates });
    if !mean {
        return Some(outer);
    }

    let out = |plan: &mut Plan, at: usize, ty: LogicalType| {
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(above, at)), ty)
    };
    let mut exprs = Vec::with_capacity(keys.len() + calls.len());
    for (at, &key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key).clone();
        exprs.push(out(plan, at, ty));
    }
    for (&call, &(sum, count)) in calls.iter().zip(&answers) {
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let summed_ty = plan.expr_type(outer_calls[sum]).clone();
        let total = out(plan, keys.len() + sum, summed_ty);
        match count {
            None => exprs.push(total),
            Some(count) => {
                let count = out(plan, keys.len() + count, LogicalType::BigInt);
                let args = plan.add_expr_list(&[total, count]);
                let name = plan.intern("__rudb_mean");
                exprs.push(plan.add_expr_at(Expr::Function { name, args }, ty, span));
            }
        }
    }
    let names: Vec<_> =
        (0..exprs.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&exprs);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input: outer, index, exprs, names }))
}
