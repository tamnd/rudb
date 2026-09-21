//! An aggregate that only reads the column it groups on is a projection over a count.
//!
//! `SELECT Referer, MIN(Referer), COUNT(*) FROM hits GROUP BY Referer` asks three questions and two
//! of them are already answered. Every row in a group holds the same referer, because that is what
//! grouping on it means, so the smallest referer in the group is the group's own key and the only
//! thing the operator has to work out is how many rows arrived. The same holds for any expression
//! over that column and nothing else: `MIN(strlen(Referer))` is `strlen` of the key, `SUM(x)` where
//! `x` is a function of the key is that function of the key times the size of the group.
//!
//! So the aggregate keeps its grouping, drops every call it had, counts rows, and a projection over
//! the top works the original answers out from the key and the count.
//!
//! # What that is worth
//!
//! `rudb-exec` answers a grouping on a stored column with a dictionary and a single `COUNT(*)` with
//! a dense array indexed by the column's storage code. Every other grouping builds a hash table. On
//! ClickBench's ninety nine million rows that is the difference between these two, measured on a
//! thirty two thread i9-13900K against the same data in the same file:
//!
//! ```text
//! SELECT Referer, COUNT(*)                  ... GROUP BY Referer    0.55 s
//! SELECT Referer, COUNT(*), MIN(Referer)    ... GROUP BY Referer    3.34 s
//! ```
//!
//! Eighty one million rows over nineteen million distinct referers, and asking one more question
//! about a group the engine already had costs six times the whole of the first query. With this pass
//! the second one is 0.56 s, because it becomes the first one with a projection on top. DuckDB
//! answers it in 3.32 s.
//!
//! # What it produces
//!
//! ```text
//! Project #1 [#2.0::VARCHAR AS column0, #2.0::VARCHAR AS column1, #2.1::BIGINT AS column2]
//!   Aggregate #2 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]
//!     <input>
//! ```
//!
//! The projection keeps the original table index and the original output order, group expressions
//! then aggregates, so nothing above it has to be rewritten. That is the shape [`crate::dependent`]
//! leaves behind and it is for the same reason. Anything the original node had a `HAVING` for reads
//! the projection's columns, and filter pushdown then moves it under the projection and onto the
//! count, so the expressions this pass writes run over the groups that survived rather than over all
//! of them.
//!
//! # Writing each call out
//!
//! `e` is the call's argument with every reference to the grouped column replaced by a reference to
//! the aggregate's key, and `n` is the count.
//!
//! | call | becomes |
//! |---|---|
//! | `count_star()` | `n` |
//! | `min(e)` | `e` |
//! | `max(e)` | `e` |
//! | `sum(e)` | `e * n` |
//! | `avg(e)` | `e * n / n` |
//!
//! `avg` is written as a total over a count and one division rather than as `e`, because that is how
//! `rudb-kernels` computes it: an exact `i128` total, a count of the rows that were not null, and one
//! division at the end. Writing the same total and the same count and dividing them the same way
//! gives the same bits. The total is `HUGEINT` so that the multiplication overflows exactly where the
//! accumulator it replaces would have, and a null `e` stays null through both of them, which is the
//! empty mean the accumulator returns.
//!
//! # What it refuses
//!
//! An aggregate with no group expressions, and one whose every call is already `count_star()`. The
//! first has no key to answer from and the second is the shape this is aiming at.
//!
//! A group expression that is not the column itself. `GROUP BY upper(Referer)` puts two different
//! referers in one group, so the group has no one value and none of this holds. A constant group
//! expression is allowed beside the column, because grouping on a column and a constant is grouping
//! on the column.
//!
//! Anything that reads two columns. `GROUP BY SearchPhrase` with `MIN(URL)` beside it is the shape
//! this cannot help, and it is the other half of issue #1076: it wants a change in the operator
//! rather than in the plan.
//!
//! `DISTINCT`, a `FILTER` on one of the calls, and `count(e)`. All three can be written out the same
//! way and none of them is what any measured query does, so they are refused here rather than written
//! untested.
//!
//! `sum` and `avg` over anything that is not an integer, because a `DOUBLE` added up n times and a
//! `DOUBLE` multiplied by n are not the same number.
//!
//! A volatile expression, which would go from once per row to once per group, and anything that is
//! not elementwise, which cannot move between two operators at all.
//!
//! # Why there is no second stage
//!
//! The obvious next step is the case where the group expression is a function of the column rather
//! than the column: group on the column first so the function runs once per distinct value, then
//! group on the function. ClickBench query 29 is that shape, and it was built and measured and it
//! lost. Grouping on the referer first and on `regexp_replace` of it second took 4.62 s against 2.49 s
//! for the plan that groups on the regular expression directly, because the first stage has to hand
//! nineteen million resolved strings to the second and that costs more than the sixty one million
//! regular expression calls it saves. The second stage is only cheap when its own grouping is cheap,
//! and when its own grouping is cheap the plan without it was never the expensive one. It is not
//! here because the measurement said not to ship it.

use rudb_common::{LogicalType, Result, Span};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan, Slice};

use crate::pass::{Context, Pass};
use crate::walk;

/// The aggregates that can be written in terms of one value and how many rows held it.
///
/// `count` is not here: see the refusals in the module documentation. An aggregate that reported
/// which row arrived first would not belong here either, because a count says how many rows a value
/// stood for and not which rows they were.
const ANSWERABLE: [&str; 5] = ["avg", "count_star", "max", "min", "sum"];

/// Turns an aggregate that only reads its own group key into a projection over a count.
#[derive(Debug, Clone, Copy)]
pub struct AnswersFromTheKey;

impl Pass for AnswersFromTheKey {
    fn name(&self) -> &'static str {
        "answers_from_the_key"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        collapse_all(plan);
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that only reads the column it groups on.
///
/// Rewrites by rebuilding, so the root moves when anything changed. What it produces is an aggregate
/// whose only call is `count_star()`, which is one of the shapes this refuses, so running it a second
/// time leaves it alone.
pub fn collapse_all(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut collapse);
    if moved {
        plan.set_root(root);
    }
}

/// The collapsed form of `at` when it is an aggregate this applies to, and nothing when it is not.
fn collapse(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let keys = plan.expr_list(groups).to_vec();
    let calls = plan.expr_list(aggregates).to_vec();
    if keys.is_empty() || calls.is_empty() {
        return None;
    }
    let mut read = keys.clone();
    for &call in &calls {
        let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else {
            return None;
        };
        if distinct || filter.is_some() || !ANSWERABLE.contains(&plan.string(name)) {
            return None;
        }
        read.extend_from_slice(plan.expr_list(args));
    }
    if calls.iter().all(|&call| counts_rows(plan, call)) {
        // This node is the shape the rewrite aims at.
        return None;
    }
    if !read.iter().all(|&expr| walk::elementwise(plan, expr) && !walk::volatile(plan, expr)) {
        return None;
    }
    let (source, reference) = source_column(plan, &read)?;
    if !keys.iter().any(|&key| holds(plan, key, source)) {
        return None;
    }
    if !keys.iter().all(|&key| holds(plan, key, source) || reads_nothing(plan, key)) {
        return None;
    }

    let below = walk::fresh_index(plan);
    let span = plan.expr_span(reference);
    let ty = plan.expr_type(reference).clone();
    let key = plan.add_expr_at(Expr::Column(source), ty.clone(), span);
    let groups = plan.add_expr_list(&[key]);
    let counted = plan.intern("count_star");
    let weight =
        Expr::Aggregate { name: counted, args: Slice::EMPTY, distinct: false, filter: None };
    let weight = plan.add_expr_at(weight, LogicalType::BigInt, span);
    let aggregates = plan.add_expr_list(&[weight]);
    let counting = plan.add_node(Node::Aggregate { input, index: below, groups, aggregates });
    let value = plan.add_expr_at(Expr::Column(ColumnBinding::new(below, 0)), ty, span);
    let count =
        plan.add_expr_at(Expr::Column(ColumnBinding::new(below, 1)), LogicalType::BigInt, span);

    let outputs = answers(plan, &keys, &calls, source, value, count)?;
    let names: Vec<_> =
        (0..outputs.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&outputs);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input: counting, index, exprs, names }))
}

/// The projection's expressions: the group expressions, then every call written out.
///
/// One row per group is one row per key, so the smallest and largest value in the group are the
/// value, `count_star` is the count, and `sum` is the value times the count. Nothing is aggregated a
/// second time.
fn answers(
    plan: &mut Plan,
    keys: &[ExprRef],
    calls: &[ExprRef],
    source: ColumnBinding,
    value: ExprRef,
    count: ExprRef,
) -> Option<Vec<ExprRef>> {
    let mut outputs: Vec<ExprRef> = Vec::with_capacity(keys.len() + calls.len());
    for &key in keys {
        let key = replace(plan, key, source, value);
        outputs.push(key);
    }
    for &call in calls {
        let Expr::Aggregate { name, args, .. } = *plan.expr(call) else { return None };
        let name = plan.string(name).to_owned();
        let want = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let argument = plan.expr_list(args).first().copied();
        let written = match name.as_str() {
            "count_star" => count,
            "min" | "max" => replace(plan, argument?, source, value),
            "sum" => {
                let each = replace(plan, argument?, source, value);
                weighted(plan, each, count, span)?
            }
            "avg" => {
                let each = replace(plan, argument?, source, value);
                let total = weighted(plan, each, count, span)?;
                divided(plan, total, count, span)?
            }
            _ => return None,
        };
        let written = cast(plan, written, &want, span);
        outputs.push(written);
    }
    Some(outputs)
}

/// The one column every expression in the run reads, and a reference to it to build a key from.
///
/// Nothing when two of them read different columns, and nothing when none of them reads any, since
/// an aggregate whose every expression is constant has no key to answer from.
fn source_column(plan: &Plan, exprs: &[ExprRef]) -> Option<(ColumnBinding, ExprRef)> {
    let mut found: Option<(ColumnBinding, ExprRef)> = None;
    let mut mixed = false;
    for &expr in exprs {
        walk::columns_at(plan, expr, &mut |at, binding| match found {
            None => found = Some((binding, at)),
            Some((held, _)) if held == binding => {}
            Some(_) => mixed = true,
        });
    }
    if mixed { None } else { found }
}

/// Whether the expression is a bare reference to the column the aggregate groups on.
fn holds(plan: &Plan, expr: ExprRef, source: ColumnBinding) -> bool {
    matches!(*plan.expr(expr), Expr::Column(binding) if binding == source)
}

/// Whether the expression reads no column at all, and so has one value for the whole query.
fn reads_nothing(plan: &Plan, expr: ExprRef) -> bool {
    let mut any = false;
    walk::columns(plan, expr, &mut |_| any = true);
    !any
}

/// Whether the call is `COUNT(*)`.
fn counts_rows(plan: &Plan, call: ExprRef) -> bool {
    matches!(*plan.expr(call), Expr::Aggregate { name, .. } if plan.string(name) == "count_star")
}

/// `expr` with every reference to `source` replaced by `value`.
///
/// The walk stops at a column rather than going through it, because what goes in its place is
/// already written against the operator below.
fn replace(plan: &mut Plan, expr: ExprRef, source: ColumnBinding, value: ExprRef) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        return if binding == source { value } else { expr };
    }
    walk::rebuild(plan, expr, &mut |plan, inner| replace(plan, inner, source, value))
}

/// `each` times `weight`, as the exact `HUGEINT` product the accumulator it replaces would hold.
///
/// Nothing unless `each` is an integer, because a `DOUBLE` added up n times is not a `DOUBLE`
/// multiplied by n.
fn weighted(plan: &mut Plan, each: ExprRef, weight: ExprRef, span: Span) -> Option<ExprRef> {
    if !plan.expr_type(each).is_integer() {
        return None;
    }
    let left = cast(plan, each, &LogicalType::HugeInt, span);
    let right = cast(plan, weight, &LogicalType::HugeInt, span);
    let product = scalar(plan, "*", &[left, right], span)?;
    (plan.expr_type(product) == &LogicalType::HugeInt).then_some(product)
}

/// `total` over `seen` as a `DOUBLE`, which is how `rudb-kernels` finishes an exact mean.
fn divided(plan: &mut Plan, total: ExprRef, seen: ExprRef, span: Span) -> Option<ExprRef> {
    let left = cast(plan, total, &LogicalType::Double, span);
    let right = cast(plan, seen, &LogicalType::Double, span);
    let ratio = scalar(plan, "/", &[left, right], span)?;
    (plan.expr_type(ratio) == &LogicalType::Double).then_some(ratio)
}

/// One scalar call, with its arguments cast to what the catalog says it takes.
fn scalar(plan: &mut Plan, name: &str, args: &[ExprRef], span: Span) -> Option<ExprRef> {
    let given: Vec<LogicalType> = args.iter().map(|&arg| plan.expr_type(arg).clone()).collect();
    let resolved = rudb_functions::resolve(name, &given).ok()?;
    if resolved.arguments.len() != args.len() {
        return None;
    }
    let mut cast_to: Vec<ExprRef> = Vec::with_capacity(args.len());
    for (&arg, wanted) in args.iter().zip(&resolved.arguments) {
        let wanted = wanted.clone();
        let arg = cast(plan, arg, &wanted, span);
        cast_to.push(arg);
    }
    let name = plan.intern(resolved.name);
    let args = plan.add_expr_list(&cast_to);
    Some(plan.add_expr_at(Expr::Function { name, args }, resolved.returns, span))
}

/// `expr` as `to`, which is `expr` itself when it is already that type.
fn cast(plan: &mut Plan, expr: ExprRef, to: &LogicalType, span: Span) -> ExprRef {
    if plan.expr_type(expr) == to {
        return expr;
    }
    plan.add_expr_at(Expr::Cast { input: expr, try_cast: false }, to.clone(), span)
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::collapse_all;

    /// What the plan a text prints looks like once the pass has run over it.
    fn collapsed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        collapse_all(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn an_extreme_over_the_group_key_becomes_the_group_key() {
        let before = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[min(#0.0::VARCHAR)::VARCHAR, count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let after = collapsed(before);
        assert!(after.contains("Project #1"), "{after}");
        assert!(
            after.contains("Aggregate #2 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]"),
            "{after}"
        );
        assert!(!after.contains("min("), "the smallest of one value is that value: {after}");
    }

    #[test]
    fn a_sum_over_the_group_key_becomes_the_key_times_its_count() {
        let before = concat!(
            "Aggregate #1 groups=[#0.1::INTEGER] aggregates=[sum(#0.1::INTEGER)::HUGEINT]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let after = collapsed(before);
        assert!(after.contains("Project #1"), "{after}");
        assert!(after.contains("\"*\"("), "{after}");
        assert!(!after.contains("sum("), "{after}");
    }

    #[test]
    fn a_mean_over_the_group_key_is_the_total_over_the_count_the_accumulator_would_have_held() {
        let before = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[avg(length(#0.0::VARCHAR)::BIGINT)::DOUBLE]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let after = collapsed(before);
        assert!(after.contains("\"*\"("), "the total carries the count: {after}");
        assert!(after.contains("\"/\"("), "one division at the end: {after}");
        assert!(!after.contains("avg("), "{after}");
    }

    #[test]
    fn an_expression_over_the_group_key_is_run_once_per_group() {
        let before = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[max(upper(#0.0::VARCHAR)::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let after = collapsed(before);
        assert!(after.contains("Project #1 [#2.0::VARCHAR AS column0"), "{after}");
        assert!(after.contains("upper(#2.0::VARCHAR)"), "{after}");
        assert!(!after.contains("max("), "{after}");
    }

    #[test]
    fn a_constant_beside_the_key_groups_the_same_rows_and_comes_back_in_place() {
        let before = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR, 7::INTEGER] aggregates=[min(#0.0::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let after = collapsed(before);
        assert!(
            after.contains("Aggregate #2 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]"),
            "the constant does not split a group: {after}"
        );
        assert!(after.contains("7::INTEGER AS column1"), "{after}");
    }

    #[test]
    fn a_grouped_count_is_left_alone_because_it_is_already_the_shape_this_aims_at() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        assert_eq!(collapsed(text), text);
    }

    #[test]
    fn a_group_key_that_is_a_function_of_the_column_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[upper(#0.0::VARCHAR)::VARCHAR] aggregates=[min(#0.0::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        assert_eq!(collapsed(text), text, "two referers can share one upper case");
    }

    #[test]
    fn an_aggregate_over_a_second_column_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[min(#0.1::INTEGER)::INTEGER]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        assert_eq!(collapsed(text), text, "the group holds one a and many bs");
    }

    #[test]
    fn an_ungrouped_aggregate_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[] aggregates=[min(#0.0::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        assert_eq!(collapsed(text), text);
    }

    #[test]
    fn a_distinct_call_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[min(DISTINCT #0.0::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        assert_eq!(collapsed(text), text);
    }

    #[test]
    fn a_sum_over_a_double_is_left_alone() {
        let text = concat!(
            "Aggregate #1 groups=[#0.2::DOUBLE] aggregates=[sum(#0.2::DOUBLE)::DOUBLE, count_star()::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER, c::DOUBLE]\n",
        );
        assert_eq!(collapsed(text), text, "n doubles added up is not a double times n");
    }

    #[test]
    fn running_it_twice_is_running_it_once() {
        let text = concat!(
            "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[min(#0.0::VARCHAR)::VARCHAR]\n",
            "  Get memory.main.t AS t #0 [a::VARCHAR, b::INTEGER]\n",
        );
        let once = collapsed(text);
        assert_ne!(once, text);
        assert_eq!(collapsed(&once), once);
    }
}
