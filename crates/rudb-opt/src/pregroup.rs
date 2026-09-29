//! Grouping by a string column first, when the groups asked for are computed from that one column.
//!
//! ClickBench q29 groups `hits` by a `regexp_replace` of `Referer`, and takes the average length of
//! `Referer`, a count and the smallest `Referer` in each group. Every one of the 8.7 million rows that
//! pass the filter looks its key up by the code of its value, reads the length of the value and its
//! rank, and adds all three into a group chosen from 400,000. There are 2.7 million distinct values
//! of `Referer`, so each value is looked up three times over on average, a cache miss each time.
//! Grouping by `Referer` first costs one hash of an integer code a row, and the grouping above it then
//! does its lookups once a value. Written out by hand that way q29 took 4.1 seconds of user time on
//! server3 against 5.8 for the query as written, with the same answer.
//!
//! # When it is the same answer
//!
//! Every group key reads one column C and nothing else, and none of them is volatile, so two rows
//! with the same C land in the same group. Grouping by C first and then by the keys computed from
//! each C puts the same rows together, and what has to be true of the aggregates is that doing them
//! in two stages adds up to doing them in one. That is the rule [`crate::eager`] follows. A count
//! and a `count(*)` come back as a `sum` of the partial counts, declared `BIGINT` so they keep their
//! type. A `sum` over integers or decimals is a sum of the partial sums, and `min` and `max` are the
//! same call again. A `min` or `max` of C itself needs no partial at all, because each group of C
//! holds one value of C, which is its key. An `avg` of an integer is two partials, a sum and a
//! count, and the average above is the one divided by the other as doubles, which is how the
//! aggregate works it out itself. A `DISTINCT` or a `FILTER` is refused, as is any other call.
//!
//! # When it is worth it
//!
//! C is a string, the file counted its distinct values, and there are at least [`SHRINK`] rows before
//! the filters for each of them, so the grouping by C is a small fraction of the work below it. At
//! least one aggregate also reads C, which is what makes each row pay for more than its key. A
//! grouping by `left(URL, 5)` with only a `count(*)` has one cheap lookup a row, and a grouping by
//! `URL` in front of it would be a table of every distinct URL built to save nothing.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #1 groups=[f(#0.0)] aggregates=[avg(g(#0.0)), count_star(), min(#0.0)]
//!   <input>
//! ```
//!
//! becomes
//!
//! ```text
//! Project #1 [#4.0, (CAST(#4.1) / CAST(#4.2)), #4.3, #4.4]
//!   Aggregate #4 groups=[f(#2.0)] aggregates=[sum(#2.1), sum(#2.2), sum(#2.3), min(#2.0)]
//!     Project #2 [#3.0, g(#3.0) * #3.1, CASE WHEN g(#3.0) IS DISTINCT FROM NULL THEN #3.1 ELSE 0 END, #3.1]
//!       Aggregate #3 groups=[#0.0] aggregates=[count_star()]
//!         <input>
//! ```
//!
//! The grouping by C only reads C, so [`crate::fromkey`] then turns it into a `count(*)` by C with
//! the partials worked out from each value and its count, which is a count by the code of each
//! value when C is stored with a dictionary. That is done here rather than left to the pass, which
//! runs before this one.
//!
//! The projection is only there when an `avg` is, and otherwise the top aggregate keeps the index.
//! Either way the top node answers under the index the aggregate had and in its order. A second run
//! finds the top aggregate reading an aggregate, or a projection over one, and leaves it.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result};
use rudb_functions::signature;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

use crate::eager::rebind;
use crate::estimate::{self, Facts};
use crate::fromkey;
use crate::pass::{Context, Pass};
use crate::walk;

/// Groups by the one column the group keys are computed from, before grouping by the keys.
#[derive(Debug, Clone, Copy)]
pub struct PreGrouping;

impl Pass for PreGrouping {
    fn name(&self) -> &'static str {
        "pre_grouping"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        pregroup(plan, context.facts());
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that this applies to.
pub fn pregroup(plan: &mut Plan, stats: &Facts) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut |plan, at| split(plan, at, stats));
    if moved {
        plan.set_root(root);
    }
}

/// How many rows each distinct value of the column has to stand for before grouping by it first is
/// worth a hash table of its own.
const SHRINK: u64 = 3;

/// What one call of the aggregate becomes.
#[derive(Clone, Copy)]
enum Split {
    /// The same call again over its partial, which is where it sits among the partials.
    Again(usize),
    /// A `sum` of the partial count, which is where it sits.
    Counted(usize),
    /// The smallest or largest of the column itself, which is the key below.
    Key,
    /// An average: the partial sum and the partial count.
    Mean(usize, usize),
}

/// The two stage form of `at` when it is an aggregate this applies to.
fn split(plan: &mut Plan, at: NodeRef, stats: &Facts) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let below = match *plan.node(input) {
        Node::Project { input, .. } => input,
        _ => input,
    };
    if matches!(plan.node(below), Node::Aggregate { .. }) {
        return None;
    }
    let keys = plan.expr_list(groups).to_vec();
    let column = only_column(plan, &keys)?;
    if keys.iter().all(|&key| *plan.expr(key) == Expr::Column(column)) {
        return None;
    }
    let (_, column_ty) =
        walk::outputs(plan, input)?.into_iter().find(|(binding, _)| *binding == column)?;
    if column_ty != LogicalType::Varchar {
        return None;
    }
    let calls = plan.expr_list(aggregates).to_vec();
    let mut reads = false;
    for &call in &calls {
        walk::columns(plan, call, &mut |binding| reads |= binding == column);
    }
    if !reads || !worth(plan, input, column, stats) {
        return None;
    }

    let mut partials: Vec<ExprRef> = Vec::new();
    let mut splits = Vec::with_capacity(calls.len());
    for &call in &calls {
        splits.push(partial(plan, call, column, &mut partials)?);
    }
    Some(rewrite(plan, input, index, column, column_ty, &keys, &calls, &partials, &splits))
}

/// The one column every group key reads, when there is exactly one and every key is elementwise and
/// not volatile.
fn only_column(plan: &Plan, keys: &[ExprRef]) -> Option<ColumnBinding> {
    let mut found: Option<ColumnBinding> = None;
    let mut more = false;
    for &key in keys {
        if !walk::elementwise(plan, key) || walk::volatile(plan, key) {
            return None;
        }
        walk::columns(plan, key, &mut |binding| match found {
            None => found = Some(binding),
            Some(seen) => more |= seen != binding,
        });
    }
    if more { None } else { found }
}

/// Whether the file says there are at least [`SHRINK`] rows under `input` for each value of
/// `column`.
fn worth(plan: &Plan, input: NodeRef, column: ColumnBinding, stats: &Facts) -> bool {
    let Some(&rows) = estimate::unfiltered(plan, input, stats).value() else { return false };
    let Some(&distinct) = estimate::stated(plan, column, stats).value() else { return false };
    rows > 0 && distinct.saturating_mul(SHRINK) <= rows
}

/// Adds what `call` needs from the grouping by the column to `partials`, and says how the call is
/// answered from them. Nothing when the call cannot be done in two stages.
fn partial(
    plan: &mut Plan,
    call: ExprRef,
    column: ColumnBinding,
    partials: &mut Vec<ExprRef>,
) -> Option<Split> {
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else { return None };
    if distinct || filter.is_some() {
        return None;
    }
    let args = plan.expr_list(args).to_vec();
    let returns = plan.expr_type(call).clone();
    let span = plan.expr_span(call);
    let at = partials.len();
    match (plan.string(name), args.as_slice()) {
        ("count_star", []) | ("count", [_]) if returns == LogicalType::BigInt => {
            partials.push(call);
            Some(Split::Counted(at))
        }
        ("min" | "max", [arg]) if *plan.expr(*arg) == Expr::Column(column) => Some(Split::Key),
        ("min" | "max", [_]) => {
            partials.push(call);
            Some(Split::Again(at))
        }
        ("sum", [arg]) => {
            let ty = plan.expr_type(*arg);
            if !(ty.is_integer() || matches!(ty, LogicalType::Decimal { .. })) {
                return None;
            }
            partials.push(call);
            Some(Split::Again(at))
        }
        ("avg", [arg]) if returns == LogicalType::Double => {
            let ty = plan.expr_type(*arg).clone();
            if !ty.is_integer() {
                return None;
            }
            let summed = signature::resolve("sum", std::slice::from_ref(&ty)).ok()?.returns;
            let only = plan.add_expr_list(&[*arg]);
            let sum = Expr::Aggregate {
                name: plan.intern("sum"),
                args: only,
                distinct: false,
                filter: None,
            };
            partials.push(plan.add_expr_at(sum, summed, span));
            let count = Expr::Aggregate {
                name: plan.intern("count"),
                args: only,
                distinct: false,
                filter: None,
            };
            partials.push(plan.add_expr_at(count, LogicalType::BigInt, span));
            Some(Split::Mean(at, at + 1))
        }
        _ => None,
    }
}

/// Puts the grouping by the column over `input` and the grouping by the keys over that.
#[expect(clippy::too_many_arguments, reason = "the pieces of one aggregate, taken apart")]
fn rewrite(
    plan: &mut Plan,
    input: NodeRef,
    index: u32,
    column: ColumnBinding,
    column_ty: LogicalType,
    keys: &[ExprRef],
    calls: &[ExprRef],
    partials: &[ExprRef],
    splits: &[Split],
) -> NodeRef {
    let below = walk::fresh_index(plan);
    let key = plan.add_expr(Expr::Column(column), column_ty.clone());
    let groups = plan.add_expr_list(&[key]);
    let aggregates = plan.add_expr_list(partials);
    let inner = plan.add_node(Node::Aggregate { input, index: below, groups, aggregates });
    // The passes before this one are done, so the grouping by the column is made a count here.
    let inner = fromkey::collapse(plan, inner).unwrap_or(inner);

    let read = |plan: &mut Plan, at: usize, ty: LogicalType| {
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(below, at)), ty)
    };
    let moved = HashMap::from([(column, ColumnBinding::new(below, 0))]);
    let outer_keys: Vec<ExprRef> = keys.iter().map(|&key| rebind(plan, key, &moved)).collect();

    let mean = splits.iter().any(|split| matches!(split, Split::Mean(..)));
    let above = if mean { walk::fresh_index(plan) } else { index };
    let mut outer_calls = Vec::new();
    // Where each call's answer is among the outer aggregates: one of them, or a sum and a count.
    let mut answers: Vec<(usize, Option<usize>)> = Vec::with_capacity(calls.len());
    let total = |plan: &mut Plan, name: &str, arg: ExprRef, ty: LogicalType, span| {
        let args = plan.add_expr_list(&[arg]);
        let call = Expr::Aggregate { name: plan.intern(name), args, distinct: false, filter: None };
        plan.add_expr_at(call, ty, span)
    };
    for (&call, &split) in calls.iter().zip(splits) {
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let Expr::Aggregate { name, .. } = *plan.expr(call) else { continue };
        let name = plan.string(name).to_owned();
        match split {
            Split::Key => {
                let arg = read(plan, 0, column_ty.clone());
                outer_calls.push(total(plan, &name, arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Again(at) => {
                let partial_ty = plan.expr_type(partials[at]).clone();
                let arg = read(plan, 1 + at, partial_ty);
                outer_calls.push(total(plan, &name, arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Counted(at) => {
                let arg = read(plan, 1 + at, LogicalType::BigInt);
                outer_calls.push(total(plan, "sum", arg, ty, span));
                answers.push((outer_calls.len() - 1, None));
            }
            Split::Mean(sum, count) => {
                let summed = plan.expr_type(partials[sum]).clone();
                let arg = read(plan, 1 + sum, summed.clone());
                outer_calls.push(total(plan, "sum", arg, summed, span));
                let arg = read(plan, 1 + count, LogicalType::BigInt);
                outer_calls.push(total(plan, "sum", arg, LogicalType::BigInt, span));
                answers.push((outer_calls.len() - 2, Some(outer_calls.len() - 1)));
            }
        }
    }
    let groups = plan.add_expr_list(&outer_keys);
    let aggregates = plan.add_expr_list(&outer_calls);
    let outer = plan.add_node(Node::Aggregate { input: inner, index: above, groups, aggregates });
    if !mean {
        return outer;
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
        match count {
            None => exprs.push(out(plan, keys.len() + sum, ty)),
            Some(count) => {
                let summed = plan.expr_type(outer_calls[sum]).clone();
                let total = out(plan, keys.len() + sum, summed);
                let seen = out(plan, keys.len() + count, LogicalType::BigInt);
                let total = plan.add_expr_at(
                    Expr::Cast { input: total, try_cast: false },
                    LogicalType::Double,
                    span,
                );
                let seen = plan.add_expr_at(
                    Expr::Cast { input: seen, try_cast: false },
                    LogicalType::Double,
                    span,
                );
                let args = plan.add_expr_list(&[total, seen]);
                let divided = Expr::Function { name: plan.intern("/"), args };
                exprs.push(plan.add_expr_at(divided, ty, span));
            }
        }
    }
    let names: Vec<_> =
        (0..exprs.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&exprs);
    let names = plan.add_name_list(&names);
    plan.add_node(Node::Project { input: outer, index, exprs, names })
}

#[cfg(test)]
mod tests {
    use rudb_common::Provenance;
    use rudb_plan::Plan;

    use super::pregroup;
    use crate::estimate::Facts;

    /// What the catalog would say about `hits`: `rows` rows and `distinct` values of `Referer`.
    fn counted(rows: u64, distinct: u64) -> Facts {
        let mut facts = Facts::new();
        facts.record("memory", "main", "hits", rows);
        facts.record_distinct(
            "memory",
            "main",
            "hits",
            "Referer",
            distinct,
            Provenance::Dictionary,
        );
        facts
    }

    /// What the plan a text prints looks like once the pass has run over it, twice.
    fn grouped(text: &str, stats: &Facts) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        pregroup(&mut plan, stats);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        pregroup(&mut plan, stats);
        assert_eq!(plan.to_string(), once, "a second run moved the plan again");
        once
    }

    /// ClickBench q29's shape.
    const HOSTS: &str = concat!(
        "Aggregate #1 groups=[regexp_replace(#0.0::VARCHAR, 'x'::VARCHAR, 'y'::VARCHAR)::VARCHAR] aggregates=[avg(strlen(#0.0::VARCHAR)::BIGINT)::DOUBLE, count_star()::BIGINT, min(#0.0::VARCHAR)::VARCHAR]\n",
        "  Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
        "    Get memory.main.hits AS hits #0 [Referer::VARCHAR]\n",
    );

    #[test]
    fn an_average_count_and_minimum_by_a_function_of_a_string_group_by_the_string_first() {
        assert_eq!(
            grouped(HOSTS, &counted(10_000_000, 2_700_000)),
            concat!(
                "Project #1 [#4.0::VARCHAR AS column0, \"/\"(CAST(#4.1::HUGEINT)::DOUBLE, CAST(#4.2::BIGINT)::DOUBLE)::DOUBLE AS column1, #4.3::BIGINT AS column2, #4.4::VARCHAR AS column3]\n",
                "  Aggregate #4 groups=[regexp_replace(#2.0::VARCHAR, 'x'::VARCHAR, 'y'::VARCHAR)::VARCHAR] aggregates=[sum(#2.1::HUGEINT)::HUGEINT, sum(#2.2::BIGINT)::BIGINT, sum(#2.3::BIGINT)::BIGINT, min(#2.0::VARCHAR)::VARCHAR]\n",
                "    Project #2 [#3.0::VARCHAR AS column0, \"*\"(CAST(strlen(#3.0::VARCHAR)::BIGINT)::HUGEINT, CAST(#3.1::BIGINT)::HUGEINT)::HUGEINT AS column1, CASE WHEN (strlen(#3.0::VARCHAR)::BIGINT IS DISTINCT FROM NULL::BIGINT)::BOOLEAN THEN #3.1::BIGINT ELSE 0::BIGINT END::BIGINT AS column2, #3.1::BIGINT AS column3]\n",
                "      Aggregate #3 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]\n",
                "        Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
                "          Get memory.main.hits AS hits #0 [Referer::VARCHAR]\n",
            )
        );
    }

    #[test]
    fn nothing_moves_without_enough_rows_for_each_value_or_without_a_count_of_them() {
        assert_eq!(grouped(HOSTS, &counted(10_000_000, 3_400_000)), HOSTS);
        assert_eq!(grouped(HOSTS, &Facts::new()), HOSTS);
    }

    #[test]
    fn a_grouping_that_only_counts_or_reads_a_second_column_stays_as_it_was() {
        let stats = counted(10_000_000, 2_700_000);
        let counts = HOSTS.replace(
            "avg(strlen(#0.0::VARCHAR)::BIGINT)::DOUBLE, count_star()::BIGINT, min(#0.0::VARCHAR)::VARCHAR",
            "count_star()::BIGINT",
        );
        assert_eq!(grouped(&counts, &stats), counts);
        let distinct = HOSTS.replace("min(#0.0::VARCHAR)", "min(DISTINCT #0.0::VARCHAR)");
        assert_eq!(grouped(&distinct, &stats), distinct);
        let bare = HOSTS.replace(
            "regexp_replace(#0.0::VARCHAR, 'x'::VARCHAR, 'y'::VARCHAR)::VARCHAR",
            "#0.0::VARCHAR",
        );
        assert_eq!(grouped(&bare, &stats), bare);
    }

    #[test]
    fn without_an_average_the_top_aggregate_keeps_the_index() {
        let text = HOSTS.replace(
            "avg(strlen(#0.0::VARCHAR)::BIGINT)::DOUBLE",
            "sum(strlen(#0.0::VARCHAR)::BIGINT)::HUGEINT, count(#0.0::VARCHAR)::BIGINT, max(upper(#0.0::VARCHAR)::VARCHAR)::VARCHAR",
        );
        assert_eq!(
            grouped(&text, &counted(10_000_000, 2_700_000)),
            concat!(
                "Aggregate #1 groups=[regexp_replace(#2.0::VARCHAR, 'x'::VARCHAR, 'y'::VARCHAR)::VARCHAR] aggregates=[sum(#2.1::HUGEINT)::HUGEINT, sum(#2.2::BIGINT)::BIGINT, max(#2.3::VARCHAR)::VARCHAR, sum(#2.4::BIGINT)::BIGINT, min(#2.0::VARCHAR)::VARCHAR]\n",
                "  Project #2 [#3.0::VARCHAR AS column0, \"*\"(CAST(strlen(#3.0::VARCHAR)::BIGINT)::HUGEINT, CAST(#3.1::BIGINT)::HUGEINT)::HUGEINT AS column1, CASE WHEN (#3.0::VARCHAR IS DISTINCT FROM NULL::VARCHAR)::BOOLEAN THEN #3.1::BIGINT ELSE 0::BIGINT END::BIGINT AS column2, upper(#3.0::VARCHAR)::VARCHAR AS column3, #3.1::BIGINT AS column4]\n",
                "    Aggregate #3 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]\n",
                "      Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
                "        Get memory.main.hits AS hits #0 [Referer::VARCHAR]\n",
            )
        );
    }
}
