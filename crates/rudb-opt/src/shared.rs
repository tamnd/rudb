//! Reading an average or a shifted sum off a sum of the same column rather than adding the column up
//! more than once.
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
//!
//! # Shifted sums
//!
//! A sum of a column moved by a constant is the sum of the column plus the constant times how many
//! rows were not null: `sum(e + c)` is `sum(e) + c * count(e)`, and `sum(e - c)` is the same with
//! the constant negated. ClickBench q30 asks for `sum(ResolutionWidth + k)` for ninety values of
//! `k`, and each one was a total of its own over ten million rows where one total and one count
//! answer all of them.
//!
//! Only whole numbers of at most 64 bits, whose sum is an exact `HUGEINT`, so the two sides are the
//! same number and not two roundings of it. A null `e` is skipped by both sides, and a group with no
//! row left has a null sum, which stays null through the addition. The one difference is that `e +
//! c` could overflow its own type on a row and raise, where the rewrite adds in `HUGEINT` and does
//! not. Casts that only widen a signed integer are taken off `e` first, because the binder writes
//! `ResolutionWidth + 1` over a `SMALLINT` as a cast to `INTEGER` plus one, and the sum of the cast
//! is the sum of the column, so a plain `sum(ResolutionWidth)` beside them shares the total.
//!
//! A lone shifted sum is left as it is, since one sum and one count cost more than one sum. It is
//! rewritten when a second shifted sum of the same argument or a plain sum of it is beside it.

use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::{fromkey, walk};

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
    /// The call was a sum of an argument moved by a constant, and is the sum of the argument, the
    /// count of it and the constant.
    Shifted { total: usize, count: usize, by: i128 },
}

/// What the first look at a call decided it wants, before any position is known.
#[derive(Clone, Copy)]
enum Want {
    Keep,
    Mean(ExprRef),
    Shift(ExprRef, i128),
}

/// The aggregate at `at` with its averages and shifted sums read off plain sums, when any can be.
fn split(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let calls = plan.expr_list(aggregates).to_vec();
    let shifts: Vec<Option<(ExprRef, i128)>> =
        calls.iter().map(|&call| shifted(plan, call)).collect();
    let mut wants = Vec::with_capacity(calls.len());
    for (at, &call) in calls.iter().enumerate() {
        if let Some(arg) = mean(plan, call)
            && calls.iter().any(|&other| summed(plan, other, arg))
        {
            wants.push(Want::Mean(arg));
            continue;
        }
        if let Some((base, by)) = shifts[at] {
            // Worth it when the one sum and one count this leaves stand for at least two calls, or
            // when the sum is there already and the count is all that is added.
            let alike = shifts
                .iter()
                .flatten()
                .filter(|&&(other, _)| walk::same(plan, other, base))
                .count();
            if alike >= 2 || calls.iter().any(|&other| summed(plan, other, base)) {
                wants.push(Want::Shift(base, by));
                continue;
            }
        }
        wants.push(Want::Keep);
    }
    if wants.iter().all(|want| matches!(want, Want::Keep)) {
        return None;
    }

    // The kept calls go first in their old order, and each sum and count this adds goes after them,
    // once for every argument however many calls read it.
    let kept: Vec<ExprRef> = calls
        .iter()
        .zip(&wants)
        .filter(|(_, want)| matches!(want, Want::Keep))
        .map(|(&call, _)| call)
        .collect();
    let mut added: Vec<ExprRef> = Vec::new();
    let mut placed = Vec::with_capacity(calls.len());
    for (&call, want) in calls.iter().zip(&wants) {
        placed.push(match *want {
            Want::Keep => Answer::Kept(kept.iter().position(|&held| held == call)?),
            Want::Mean(arg) => Answer::Mean {
                total: total_of(plan, &kept, &mut added, arg),
                count: count_of(plan, &kept, &mut added, arg),
            },
            Want::Shift(base, by) => Answer::Shifted {
                total: total_of(plan, &kept, &mut added, base),
                count: count_of(plan, &kept, &mut added, base),
                by,
            },
        });
    }
    let built: Vec<ExprRef> = kept.iter().chain(&added).copied().collect();

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
        let span = plan.expr_span(call);
        let expr = match *answer {
            Answer::Kept(at) => column(plan, keys.len() + at, ty),
            Answer::Mean { total, count } => {
                let summed = plan.expr_type(built[total]).clone();
                let total = column(plan, keys.len() + total, summed);
                let count = column(plan, keys.len() + count, LogicalType::BigInt);
                let name = plan.intern("__rudb_mean");
                let args = plan.add_expr_list(&[total, count]);
                plan.add_expr_at(Expr::Function { name, args }, ty, span)
            }
            Answer::Shifted { total, count, by } => {
                let total = column(plan, keys.len() + total, LogicalType::HugeInt);
                let count = column(plan, keys.len() + count, LogicalType::BigInt);
                let count = fromkey::cast(plan, count, &LogicalType::HugeInt, span);
                let by = plan.add_value(Value::HugeInt(by));
                let by = plan.add_expr_at(Expr::Constant(by), LogicalType::HugeInt, span);
                let moved = fromkey::scalar(plan, "*", &[by, count], span)?;
                let sum = fromkey::scalar(plan, "+", &[total, moved], span)?;
                if plan.expr_type(sum) != &ty {
                    return None;
                }
                sum
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

/// The position of a plain sum of `arg` among the kept calls and then the added ones, adding one
/// when neither has it.
fn total_of(plan: &mut Plan, kept: &[ExprRef], added: &mut Vec<ExprRef>, arg: ExprRef) -> usize {
    if let Some(at) = kept.iter().position(|&call| summed(plan, call, arg)) {
        return at;
    }
    if let Some(at) = added.iter().position(|&call| summed(plan, call, arg)) {
        return kept.len() + at;
    }
    added.push(aggregate(plan, "sum", arg, LogicalType::HugeInt));
    kept.len() + added.len() - 1
}

/// The position of a plain count of `arg` among the added calls, adding one when there is none.
fn count_of(plan: &mut Plan, kept: &[ExprRef], added: &mut Vec<ExprRef>, arg: ExprRef) -> usize {
    let counts = |plan: &Plan, call: ExprRef| {
        plain(plan, call, "count").is_some_and(|counted| walk::same(plan, counted, arg))
    };
    if let Some(at) = added.iter().position(|&call| counts(plan, call)) {
        return kept.len() + at;
    }
    added.push(aggregate(plan, "count", arg, LogicalType::BigInt));
    kept.len() + added.len() - 1
}

/// The argument and the constant of a plain `sum(e + c)`, `sum(c + e)` or `sum(e - c)` over whole
/// numbers, with the constant negated for the last, and `None` for any other call.
///
/// The argument has the casts that only widen an integer taken off it, since the binder writes
/// `ResolutionWidth + 1` over a `SMALLINT` as a cast to `INTEGER` plus one, and the sum of the
/// cast is the sum of the column. That is what lets `sum(x)` beside `sum(x + 1)` share one total.
fn shifted(plan: &Plan, call: ExprRef) -> Option<(ExprRef, i128)> {
    if plan.expr_type(call) != &LogicalType::HugeInt {
        return None;
    }
    let arg = plain(plan, call, "sum")?;
    let Expr::Function { name, args } = *plan.expr(arg) else { return None };
    let [left, right] = *plan.expr_list(args) else { return None };
    let (base, by) = match (plan.string(name), constant(plan, left), constant(plan, right)) {
        ("+", None, Some(by)) => (left, by),
        ("+", Some(by), None) => (right, by),
        ("-", None, Some(by)) => (left, -by),
        _ => return None,
    };
    let base = widened(plan, base);
    small(plan.expr_type(arg)).then_some(())?;
    small(plan.expr_type(base)).then_some((base, by))
}

/// The value of an integer constant that is not null.
fn constant(plan: &Plan, expr: ExprRef) -> Option<i128> {
    let Expr::Constant(value) = *plan.expr(expr) else { return None };
    plan.value(value).as_i64().map(i128::from)
}

/// `expr` with every cast that takes a signed integer to a wider one taken off the top of it.
fn widened(plan: &Plan, mut expr: ExprRef) -> ExprRef {
    while let Expr::Cast { input, try_cast: false } = *plan.expr(expr) {
        match (rank(plan.expr_type(input)), rank(plan.expr_type(expr))) {
            (Some(from), Some(to)) if from <= to => expr = input,
            _ => break,
        }
    }
    expr
}

/// Where a signed integer type of at most 64 bits sits in order of width.
fn rank(ty: &LogicalType) -> Option<u8> {
    match ty {
        LogicalType::TinyInt => Some(0),
        LogicalType::SmallInt => Some(1),
        LogicalType::Integer => Some(2),
        LogicalType::BigInt => Some(3),
        _ => None,
    }
}

/// Whether a sum of this type adds into an exact `HUGEINT` that the row count cannot overflow.
fn small(ty: &LogicalType) -> bool {
    ty.is_integer() && !matches!(ty, LogicalType::HugeInt | LogicalType::UHugeInt)
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

/// A plain call to the aggregate `name` over `arg`.
fn aggregate(plan: &mut Plan, name: &str, arg: ExprRef, ty: LogicalType) -> ExprRef {
    let name = plan.intern(name);
    let args = plan.add_expr_list(&[arg]);
    plan.add_expr(Expr::Aggregate { name, args, distinct: false, filter: None }, ty)
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

    const SHIFTED: &str = concat!(
        "Aggregate #1 groups=[] aggregates=[sum(#0.0::SMALLINT)::HUGEINT, ",
        "sum(\"+\"(CAST(#0.0::SMALLINT)::INTEGER, 1::INTEGER)::INTEGER)::HUGEINT, ",
        "sum(\"+\"(CAST(#0.0::SMALLINT)::INTEGER, 2::INTEGER)::INTEGER)::HUGEINT]\n",
        "  Get memory.main.t AS t #0 [w::SMALLINT]\n",
    );

    #[test]
    fn sums_moved_by_constants_are_read_off_one_sum_and_a_count() {
        assert_eq!(
            shared(SHIFTED),
            concat!(
                "Project #1 [#2.0::HUGEINT AS column0, ",
                "\"+\"(#2.0::HUGEINT, \"*\"(1::HUGEINT, CAST(#2.1::BIGINT)::HUGEINT)::HUGEINT)::HUGEINT AS column1, ",
                "\"+\"(#2.0::HUGEINT, \"*\"(2::HUGEINT, CAST(#2.1::BIGINT)::HUGEINT)::HUGEINT)::HUGEINT AS column2]\n",
                "  Aggregate #2 groups=[] aggregates=[sum(#0.0::SMALLINT)::HUGEINT, count(#0.0::SMALLINT)::BIGINT]\n",
                "    Get memory.main.t AS t #0 [w::SMALLINT]\n",
            )
        );
    }

    #[test]
    fn a_difference_beside_the_plain_sum_is_read_off_it() {
        let minus = concat!(
            "Aggregate #1 groups=[#0.1::INTEGER] aggregates=[sum(#0.0::BIGINT)::HUGEINT, ",
            "sum(\"-\"(#0.0::BIGINT, 3::BIGINT)::BIGINT)::HUGEINT]\n",
            "  Get memory.main.t AS t #0 [v::BIGINT, g::INTEGER]\n",
        );
        let after = shared(minus);
        assert!(after.contains("\"*\"(-3::HUGEINT"), "{after}");
        assert!(
            after.contains("aggregates=[sum(#0.0::BIGINT)::HUGEINT, count(#0.0::BIGINT)::BIGINT]"),
            "{after}"
        );
    }

    #[test]
    fn one_shifted_sum_or_a_shift_of_a_double_is_left_alone() {
        let alone = concat!(
            "Aggregate #1 groups=[] aggregates=[",
            "sum(\"+\"(CAST(#0.0::SMALLINT)::INTEGER, 1::INTEGER)::INTEGER)::HUGEINT]\n",
            "  Get memory.main.t AS t #0 [w::SMALLINT]\n",
        );
        assert_eq!(shared(alone), alone);
        let doubles = concat!(
            "Aggregate #1 groups=[] aggregates=[sum(\"+\"(#0.0::DOUBLE, 1.0::DOUBLE)::DOUBLE)::DOUBLE, ",
            "sum(\"+\"(#0.0::DOUBLE, 2.0::DOUBLE)::DOUBLE)::DOUBLE]\n",
            "  Get memory.main.t AS t #0 [w::DOUBLE]\n",
        );
        assert_eq!(shared(doubles), doubles);
    }
}
