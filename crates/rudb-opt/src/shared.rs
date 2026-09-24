//! Reading an average off a sum of the same column rather than adding the column up twice.
//!
//! TPC-H q01 asks for `sum(l_quantity)` and `avg(l_quantity)`, and `sum(l_extendedprice)` and
//! `avg(l_extendedprice)`, in one grouping. Each `avg` keeps a total of its own and a count, so the
//! scan's six million rows were added into four totals where two would do. The sum and the average
//! were a seventh of the samples on q01 between them, which is most of what q01 costs over DuckDB.
//!
//! An average is its total over its count. When the aggregate already sums the same argument, the
//! average becomes that sum, a `count` of the argument, and a projection above that divides one by
//! the other. A count over a column with no nulls in it is taken off the chunk's group tally rather
//! than off the rows, so it is close to free, and the sum was being paid for anyway.
//!
//! # When it is the same answer
//!
//! `avg` over a whole number column adds into an exact `i128` and divides once at the end, by the
//! count times the power of ten the scale says, and `__rudb_mean` does that same division through
//! the same function, so the bits are the same. That holds while the exact total does not overflow,
//! and `avg` quietly goes on in floating point where `sum` raises. So the argument has to be an
//! integer of at most 64 bits or a decimal that fits in one, where the total cannot overflow before
//! the row count passes two to the sixty four. A `DISTINCT` or a `FILTER` on either call is refused,
//! since the two would then be over different rows.
//!
//! An average with no sum of the same argument beside it is left alone, because a sum and a count
//! cost at least what a mean does.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #2 groups=[#0.0] aggregates=[sum(#0.1), avg(#0.1)]
//! ```
//!
//! becomes
//!
//! ```text
//! Project #2 [#3.0 AS column0, #3.1 AS column1, __rudb_mean(#3.1, #3.2) AS column2]
//!   Aggregate #3 groups=[#0.0] aggregates=[sum(#0.1), count(#0.1)]
//! ```
//!
//! The projection takes the aggregate's index and its output order, so nothing above it moves. A
//! second run finds no `avg` beside a sum and stops.

use rudb_common::{LogicalType, Result};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::walk;

/// Answers an average from a sum of the same argument and a count.
#[derive(Debug, Clone, Copy)]
pub struct CommonAggregate;

impl Pass for CommonAggregate {
    fn name(&self) -> &'static str {
        "common_aggregate"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        share(plan);
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that this applies to.
pub fn share(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut split);
    if moved {
        plan.set_root(root);
    }
}

/// What one of the aggregate's calls becomes.
#[derive(Clone, Copy)]
enum Answer {
    /// The call is kept, at this position among the new calls.
    Kept(usize),
    /// The call was an average, and is the sum and the count at these positions.
    Mean { total: usize, count: usize },
}

/// The aggregate at `at` with its averages read off sums, when any of them can be.
fn split(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let calls = plan.expr_list(aggregates).to_vec();
    let mut kept: Vec<ExprRef> = Vec::with_capacity(calls.len());
    let mut answers = Vec::with_capacity(calls.len());
    let mut shared = false;
    for &call in &calls {
        let Some((arg, total)) = mean(plan, call).and_then(|arg| {
            let total = calls.iter().position(|&other| summed(plan, other, arg))?;
            Some((arg, total))
        }) else {
            answers.push(None);
            kept.push(call);
            continue;
        };
        shared = true;
        answers.push(Some((arg, total)));
    }
    if !shared {
        return None;
    }

    // The kept calls go first in their old order, and each count goes after them, once for every
    // argument however many averages read it.
    let mut placed = Vec::with_capacity(calls.len());
    let mut counts: Vec<(ExprRef, usize)> = Vec::new();
    for (&call, answer) in calls.iter().zip(&answers) {
        placed.push(match *answer {
            None => Answer::Kept(kept.iter().position(|&held| held == call)?),
            Some((arg, total)) => {
                let total = kept.iter().position(|&held| held == calls[total])?;
                let count = match counts.iter().find(|(held, _)| walk::same(plan, *held, arg)) {
                    Some(&(_, count)) => count,
                    None => {
                        let count = kept.len() + counts.len();
                        counts.push((arg, count));
                        count
                    }
                };
                Answer::Mean { total, count }
            }
        });
    }
    let mut built = kept.clone();
    for &(arg, _) in &counts {
        built.push(count(plan, arg));
    }

    let staged = walk::fresh_index(plan);
    let keys = plan.expr_list(groups).to_vec();
    let aggregates = plan.add_expr_list(&built);
    let inner = plan.add_node(Node::Aggregate { input, index: staged, groups, aggregates });

    let column = |plan: &mut Plan, at: usize, ty: LogicalType| {
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(staged, at)), ty)
    };
    let mut projected = Vec::with_capacity(keys.len() + calls.len());
    for (at, &key) in keys.iter().enumerate() {
        let ty = plan.expr_type(key).clone();
        projected.push(column(plan, at, ty));
    }
    for (&call, answer) in calls.iter().zip(&placed) {
        let ty = plan.expr_type(call).clone();
        let expr = match *answer {
            Answer::Kept(at) => column(plan, keys.len() + at, ty),
            Answer::Mean { total, count } => {
                let summed = plan.expr_type(kept[total]).clone();
                let total = column(plan, keys.len() + total, summed);
                let count = column(plan, keys.len() + count, LogicalType::BigInt);
                let name = plan.intern("__rudb_mean");
                let args = plan.add_expr_list(&[total, count]);
                let span = plan.expr_span(call);
                plan.add_expr_at(Expr::Function { name, args }, ty, span)
            }
        };
        projected.push(expr);
    }
    let names: Vec<_> =
        (0..projected.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&projected);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input: inner, index, exprs, names }))
}

/// The argument of a plain `avg` this can answer from a sum, and `None` for any other call.
fn mean(plan: &Plan, call: ExprRef) -> Option<ExprRef> {
    let arg = plain(plan, call, "avg")?;
    if plan.expr_type(call) != &LogicalType::Double {
        return None;
    }
    let fits = match plan.expr_type(arg) {
        LogicalType::Decimal { width, .. } => *width <= 18,
        ty => ty.is_integer() && !matches!(ty, LogicalType::HugeInt | LogicalType::UHugeInt),
    };
    fits.then_some(arg)
}

/// Whether `call` is a plain `sum` of `arg` whose total `__rudb_mean` can divide.
fn summed(plan: &Plan, call: ExprRef, arg: ExprRef) -> bool {
    let Some(summing) = plain(plan, call, "sum") else { return false };
    let total = match (plan.expr_type(call), plan.expr_type(arg)) {
        (LogicalType::HugeInt, from) => from.is_integer(),
        (LogicalType::Decimal { scale, .. }, LogicalType::Decimal { scale: from, .. }) => {
            scale == from
        }
        _ => false,
    };
    total && walk::same(plan, summing, arg)
}

/// The one argument of a call to `name` with no `DISTINCT` and no `FILTER`.
fn plain(plan: &Plan, call: ExprRef, name: &str) -> Option<ExprRef> {
    let Expr::Aggregate { name: called, args, distinct, filter } = *plan.expr(call) else {
        return None;
    };
    let [arg] = plan.expr_list(args) else { return None };
    (!distinct && filter.is_none() && plan.string(called) == name).then_some(*arg)
}

/// A `count` of `arg`.
fn count(plan: &mut Plan, arg: ExprRef) -> ExprRef {
    let name = plan.intern("count");
    let args = plan.add_expr_list(&[arg]);
    plan.add_expr(
        Expr::Aggregate { name, args, distinct: false, filter: None },
        LogicalType::BigInt,
    )
}

#[cfg(test)]
mod tests {
    use rudb_plan::Plan;

    use super::share;

    /// What the plan a text prints looks like once the pass has run over it, twice.
    fn shared(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        share(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        share(&mut plan);
        assert_eq!(plan.to_string(), once, "a second run changed the plan again");
        once
    }

    const BOTH: &str = concat!(
        "Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[avg(#0.1::DECIMAL(15,2))::DOUBLE, ",
        "sum(#0.1::DECIMAL(15,2))::DECIMAL(38,2), avg(#0.2::INTEGER)::DOUBLE]\n",
        "  Get memory.main.t AS t #0 [g::VARCHAR, price::DECIMAL(15,2), n::INTEGER]\n",
    );

    #[test]
    fn an_average_beside_a_sum_of_the_same_column_is_read_off_it() {
        assert_eq!(
            shared(BOTH),
            concat!(
                "Project #1 [#2.0::VARCHAR AS column0, __rudb_mean(#2.1::DECIMAL(38,2), ",
                "#2.3::BIGINT)::DOUBLE AS column1, #2.1::DECIMAL(38,2) AS column2, ",
                "#2.2::DOUBLE AS column3]\n",
                "  Aggregate #2 groups=[#0.0::VARCHAR] aggregates=[sum(#0.1::DECIMAL(15,2))::DECIMAL(38,2), ",
                "avg(#0.2::INTEGER)::DOUBLE, count(#0.1::DECIMAL(15,2))::BIGINT]\n",
                "    Get memory.main.t AS t #0 [g::VARCHAR, price::DECIMAL(15,2), n::INTEGER]\n",
            )
        );
    }

    #[test]
    fn an_average_with_no_sum_beside_it_or_a_filter_is_left_alone() {
        let alone = BOTH.replace("sum(#0.1::DECIMAL(15,2))", "sum(#0.2::INTEGER)");
        let alone = alone.replace("::DECIMAL(38,2), avg(#0.2", "::HUGEINT, max(#0.2");
        assert_eq!(shared(&alone), alone);
        let distinct =
            BOTH.replace("avg(#0.1::DECIMAL(15,2))", "avg(DISTINCT #0.1::DECIMAL(15,2))");
        assert_eq!(shared(&distinct), distinct);
    }

    #[test]
    fn a_wide_decimal_is_left_alone() {
        let wide = BOTH.replace("DECIMAL(15,2)", "DECIMAL(38,2)");
        assert_eq!(shared(&wide), wide);
    }
}
