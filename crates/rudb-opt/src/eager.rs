//! Summing below a join instead of above it, when the join only brings in wide columns to group by.
//!
//! TPC-H q10 joins customer, orders, lineitem and nation and then groups on seven columns, four of
//! them strings off customer and nation, to sum a lineitem expression. The grouping sees every
//! joined row, which at SF1 is 114,705 rows each carrying four strings to hash and compare, and it
//! ends with 37,967 groups, one per customer. Summing by `o_custkey` below the join to customer
//! gives the same 37,967 rows before a single string is read, and the join and the grouping above
//! it then do a third of the work. DuckDB does not do this and spends about as long on q10 as rudb
//! did before it, so this is one of the places where doing less work is how the gap opens.
//!
//! # When it is the same answer
//!
//! An aggregate over a path of inner joins and filters, with every argument read from one input B
//! somewhere down that path. Call K the columns of B that anything on the path or the group list
//! reads. Two rows of B with the same K meet the same rows on every join above, pass or fail the
//! same filters, and land in the same group, so summing them first and then summing the sums is
//! the sum. `min` and `max` are the same argument, and a null that made a partial sum null is
//! skipped above the way it was skipped below. Nothing is said about duplicates on the other side
//! of a join, because a partial row is repeated by a match exactly the way the rows it stands for
//! would have been.
//!
//! `count` and `count(*)` come back as a `sum` of the partial counts, declared `BIGINT` so the total
//! keeps the type the count had. `avg` would have to be split in two, so it is left for later rather
//! than half done here. `sum` over a float is refused because adding in a different order is a
//! different answer there. A `DISTINCT` or a `FILTER` is refused because neither survives being done
//! twice. At least one call has to read a column, since a lone `count(*)` does not say which input
//! is B.
//!
//! A left join is on the path as well as an inner one. Where B is under the side every row of which
//! is kept, it is the inner case, because each row of B still meets the same rows. Where B is under
//! the side that is padded with nulls, a row of the other side that matched nothing comes up with a
//! null where the partial aggregate would be. A `sum`, `min` or `max` of it is null either way, but
//! a count of it was 0 and a `count(*)` of it was 1, so those two read `coalesce(partial, 0)` and
//! `coalesce(partial, 1)` above the join.
//!
//! # When it is worth it
//!
//! Where the join above B brings in a string the grouping has to hash, or where grouping B by K
//! leaves at most a quarter of its rows, and in both cases where K is a handful of fixed width
//! columns. The other input of that join has to be a scan with nothing filtered, so the
//! join brings columns in rather than throwing rows away. Where it throws most of them away, as the
//! join to supplier does in q05, the partial sum would read every row the join was about to drop,
//! and it would sit in the way of the runtime filter that join sends down to the lineitem scan.
//! Measured on q05 before this rule, the partial sum made the query cost two and a half times the
//! instructions it had. The string is the case where the partial grouping is cheap per row and the
//! grouping it saves was expensive per row. The quarter is the case where the join and everything
//! above it see a quarter of the rows or fewer. That one is only claimed where B is a scan under
//! filters and the file counted the distinct values of every column of K, which is what a native
//! table's sketches say, and the product of those counts is taken as the number of groups. TPC-H
//! q13 is the example: `orders` grouped by `o_custkey` is about 100,000 rows out of 1.5 million, so
//! the left join to `customer` and the grouping above it stop seeing every order. The walk tries the highest B first and goes down one join at a time, so on
//! q10 it tries the side under nation, whose K would hold customer's strings, and then the side
//! under customer, whose K is `o_custkey`.
//!
//! # What it produces
//!
//! ```text
//! Aggregate #4 groups=[c.name, n.name] aggregates=[sum(l.price)]
//!   Join INNER on=[c.key = o.cust]
//!     Get customer #0
//!     <B>
//! ```
//!
//! becomes
//!
//! ```text
//! Aggregate #4 groups=[c.name, n.name] aggregates=[sum(#5.1)]
//!   Join INNER on=[c.key = #5.0]
//!     Get customer #0
//!     Aggregate #5 groups=[o.cust] aggregates=[sum(l.price)]
//!       <B>
//! ```
//!
//! The top aggregate keeps its index and its output order, so nothing above it moves. A second run
//! finds an aggregate where B was and stops, so the pass settles after one.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result, Value};
use rudb_plan::{ColumnBinding, Expr, ExprRef, JoinKind, Node, NodeRef, Plan};

use crate::pass::{Context, Pass};
use crate::walk;

/// Moves a sum below the joins that only bring in columns to group by.
#[derive(Debug, Clone, Copy)]
pub struct EagerAggregation;

impl Pass for EagerAggregation {
    fn name(&self) -> &'static str {
        "eager_aggregation"
    }

    fn run(&self, plan: &mut Plan, _context: &Context) -> Result<()> {
        push(plan);
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that this applies to.
pub fn push(plan: &mut Plan) {
    let mut moved = false;
    let root = walk::restack(plan, plan.root(), &mut moved, &mut split);
    if moved {
        plan.set_root(root);
    }
}

/// One node on the way down from the aggregate to B.
#[derive(Clone, Copy)]
enum Step {
    /// A filter, whose predicate may read B.
    Filter { predicate: ExprRef },
    /// An inner join, and whether B is under its left input.
    Join { at: NodeRef, left: bool },
}

/// The two stage form of `at` when it is an aggregate this applies to.
fn split(plan: &mut Plan, at: NodeRef) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let calls = plan.expr_list(aggregates).to_vec();
    if calls.is_empty() || !calls.iter().all(|&call| movable(plan, call)) {
        return None;
    }
    let keys = plan.expr_list(groups).to_vec();
    let mut read = Vec::new();
    for &call in &calls {
        walk::columns(plan, call, &mut |binding| read.push(binding));
    }
    if read.is_empty() {
        return None;
    }

    let mut path: Vec<Step> = Vec::new();
    let mut padded = false;
    let mut here = input;
    loop {
        match *plan.node(here) {
            Node::Filter { input, predicate } => {
                path.push(Step::Filter { predicate });
                here = input;
            }
            Node::Join { left, right, kind: kind @ (JoinKind::Inner | JoinKind::Left), .. } => {
                let left_side = walk::outputs(plan, left)?;
                let right_side = walk::outputs(plan, right)?;
                let within = |side: &[(ColumnBinding, LogicalType)]| {
                    read.iter().all(|binding| side.iter().any(|(found, _)| found == binding))
                };
                let (below, beside, side, other) = if within(&left_side) {
                    (left, right, left_side, right_side)
                } else if within(&right_side) {
                    (right, left, right_side, left_side)
                } else {
                    return None;
                };
                path.push(Step::Join { at: here, left: below == left });
                padded |= kind == JoinKind::Left && below == right;
                let scan = matches!(plan.node(beside), Node::Get { .. });
                if let Some(kept) =
                    scan.then(|| worth(plan, &path, &keys, below, &side, &other)).flatten()
                {
                    let staged = Staged { below, kept, padded };
                    return Some(rewrite(plan, &path, staged, index, &keys, &calls));
                }
                here = below;
            }
            _ => return None,
        }
    }
}

/// Whether a call can be done in two stages and keep its name, its argument and its type.
fn movable(plan: &Plan, call: ExprRef) -> bool {
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else { return false };
    if distinct || filter.is_some() {
        return false;
    }
    let name = plan.string(name);
    let args = plan.expr_list(args);
    if name == "count_star" {
        return args.is_empty() && plan.expr_type(call) == &LogicalType::BigInt;
    }
    let [arg] = args else { return false };
    match name {
        "sum" => matches!(plan.expr_type(*arg), LogicalType::Decimal { .. }),
        "count" => plan.expr_type(call) == &LogicalType::BigInt,
        "min" | "max" => true,
        _ => false,
    }
}

/// The columns of B to group the partial sums by, when splitting at `below` pays.
///
/// Every column of B that a group key or anything on the path reads, in the order B produces them.
/// Nothing when B is an aggregate already, which is what makes a second run stop, when a column of
/// B that has to be kept is not a fixed width one, or when nothing the join brings in to group by
/// is a string.
fn worth(
    plan: &Plan,
    path: &[Step],
    keys: &[ExprRef],
    below: NodeRef,
    side: &[(ColumnBinding, LogicalType)],
    other: &[(ColumnBinding, LogicalType)],
) -> Option<Vec<(ColumnBinding, LogicalType)>> {
    if matches!(plan.node(below), Node::Aggregate { .. }) {
        return None;
    }
    let mut wanted = Vec::new();
    let mut wide = false;
    for &key in keys {
        walk::columns(plan, key, &mut |binding| {
            wanted.push(binding);
            if other.iter().any(|(found, ty)| *found == binding && ty == &LogicalType::Varchar) {
                wide = true;
            }
        });
    }
    for step in path {
        match *step {
            Step::Filter { predicate } => {
                walk::columns(plan, predicate, &mut |binding| wanted.push(binding));
            }
            Step::Join { at, .. } => {
                let Node::Join { conditions, .. } = *plan.node(at) else { return None };
                for &condition in plan.expr_list(conditions) {
                    walk::columns(plan, condition, &mut |binding| wanted.push(binding));
                }
            }
        }
    }
    let kept: Vec<(ColumnBinding, LogicalType)> =
        side.iter().filter(|(binding, _)| wanted.contains(binding)).cloned().collect();
    let narrow = |ty: &LogicalType| {
        ty.is_integer() || ty.is_temporal() || matches!(ty, LogicalType::Decimal { .. })
    };
    let fits = !kept.is_empty() && kept.iter().all(|(_, ty)| narrow(ty));
    (fits && (wide || shrinks(plan, below, &kept))).then_some(kept)
}

/// How many times fewer rows B has to come out with once grouped by K before that alone is worth
/// the partial aggregate.
const SHRINK: u64 = 4;

/// Whether grouping B by `kept` leaves at most one row in [`SHRINK`], by what the file says.
///
/// Only for a B that is one scan under filters, whose row count and whose distinct count for every
/// column of K were measured. The groups are taken as the product of those counts, capped at the
/// rows, which is the most there can be. The filters are not counted, because a filter that drops
/// rows drops groups with them and the ratio the scan had is the best there is to go on.
fn shrinks(plan: &Plan, below: NodeRef, kept: &[(ColumnBinding, LogicalType)]) -> bool {
    let mut here = below;
    while let Node::Filter { input, .. } = *plan.node(here) {
        here = input;
    }
    let Node::Get { index, columns, .. } = *plan.node(here) else { return false };
    let Some(&rows) = plan.measured(index).value() else { return false };
    let fields = plan.field_list(columns);
    let mut groups: u64 = 1;
    for (binding, _) in kept {
        if binding.table != index {
            return false;
        }
        let Some(field) = fields.get(binding.column as usize) else { return false };
        let Some(&distinct) = plan.distinct_measured(index, &field.name).value() else {
            return false;
        };
        groups = groups.saturating_mul(distinct);
    }
    rows > 0 && groups.min(rows).saturating_mul(SHRINK) <= rows
}

/// Where the partial aggregate goes and what it groups by.
struct Staged {
    /// B.
    below: NodeRef,
    /// K, in the order B produces it.
    kept: Vec<(ColumnBinding, LogicalType)>,
    /// Whether a left join on the path pads B's side with nulls, which is what makes a count read
    /// `coalesce` above it.
    padded: bool,
}

/// Puts the partial aggregate over `below` and rebuilds the path above it to read from it.
fn rewrite(
    plan: &mut Plan,
    path: &[Step],
    staged_at: Staged,
    index: u32,
    keys: &[ExprRef],
    calls: &[ExprRef],
) -> NodeRef {
    let Staged { below, kept, padded } = staged_at;
    let staged = walk::fresh_index(plan);
    let column = |plan: &mut Plan, at: usize, ty: LogicalType| {
        let at = u32::try_from(at).expect("an aggregate with this many expressions cannot bind");
        plan.add_expr(Expr::Column(ColumnBinding::new(staged, at)), ty)
    };

    let mut moved = HashMap::new();
    let mut groups = Vec::with_capacity(kept.len());
    for (at, (binding, ty)) in kept.iter().enumerate() {
        groups.push(plan.add_expr(Expr::Column(*binding), ty.clone()));
        moved.insert(*binding, ColumnBinding::new(staged, u32::try_from(at).unwrap_or(u32::MAX)));
    }
    let groups = plan.add_expr_list(&groups);
    let partials = plan.add_expr_list(calls);
    let mut built = plan.add_node(Node::Aggregate {
        input: below,
        index: staged,
        groups,
        aggregates: partials,
    });

    for step in path.iter().rev() {
        let node = match *step {
            Step::Filter { predicate } => {
                Node::Filter { input: built, predicate: rebind(plan, predicate, &moved) }
            }
            Step::Join { at, left } => {
                let mut node = plan.node(at).clone();
                if let Node::Join { left: l, right: r, conditions, .. } = &mut node {
                    if left {
                        *l = built;
                    } else {
                        *r = built;
                    }
                    let held = plan.expr_list(*conditions).to_vec();
                    let rebound: Vec<ExprRef> =
                        held.iter().map(|&condition| rebind(plan, condition, &moved)).collect();
                    if rebound != held {
                        *conditions = plan.add_expr_list(&rebound);
                    }
                }
                node
            }
        };
        built = plan.add_node(node);
    }

    let outer_keys: Vec<ExprRef> = keys.iter().map(|&key| rebind(plan, key, &moved)).collect();
    let mut outer_calls = Vec::with_capacity(calls.len());
    for (at, &call) in calls.iter().enumerate() {
        let Expr::Aggregate { name, .. } = *plan.expr(call) else { continue };
        let ty = plan.expr_type(call).clone();
        let span = plan.expr_span(call);
        let mut arg = column(plan, kept.len() + at, ty.clone());
        let (name, nothing) = match plan.string(name) {
            "count" => (plan.intern("sum"), Some(0)),
            "count_star" => (plan.intern("sum"), Some(1)),
            _ => (name, None),
        };
        if let Some(nothing) = nothing.filter(|_| padded) {
            let nothing = plan.add_constant(Value::BigInt(nothing));
            let args = plan.add_expr_list(&[arg, nothing]);
            let coalesce = Expr::Function { name: plan.intern("coalesce"), args };
            arg = plan.add_expr_at(coalesce, ty.clone(), span);
        }
        let args = plan.add_expr_list(&[arg]);
        let total = Expr::Aggregate { name, args, distinct: false, filter: None };
        outer_calls.push(plan.add_expr_at(total, ty, span));
    }
    let groups = plan.add_expr_list(&outer_keys);
    let aggregates = plan.add_expr_list(&outer_calls);
    plan.add_node(Node::Aggregate { input: built, index, groups, aggregates })
}

/// `expr` with every column of B it reads pointed at the partial aggregate's column for it.
fn rebind(
    plan: &mut Plan,
    expr: ExprRef,
    moved: &HashMap<ColumnBinding, ColumnBinding>,
) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        return match moved.get(&binding) {
            Some(&to) => {
                let ty = plan.expr_type(expr).clone();
                let span = plan.expr_span(expr);
                plan.add_expr_at(Expr::Column(to), ty, span)
            }
            None => expr,
        };
    }
    walk::rebuild(plan, expr, &mut |plan, child| rebind(plan, child, moved))
}

#[cfg(test)]
mod tests {
    use rudb_common::{Provenance, Stat};
    use rudb_plan::Plan;

    use super::push;

    /// What the plan a text prints looks like once the pass has run over it, twice.
    fn pushed(text: &str) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        push(&mut plan);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        push(&mut plan);
        assert_eq!(plan.to_string(), once, "a second run moved the plan again");
        once
    }

    const JOINED: &str = concat!(
        "Aggregate #3 groups=[#0.1::VARCHAR] aggregates=[sum(#1.1::DECIMAL(15,2))::DECIMAL(38,2)]\n",
        "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
        "    Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR]\n",
        "    Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
    );

    #[test]
    fn a_sum_moves_below_a_join_that_brings_in_a_string_to_group_by() {
        assert_eq!(
            pushed(JOINED),
            concat!(
                "Aggregate #3 groups=[#0.1::VARCHAR] aggregates=[sum(#4.1::DECIMAL(38,2))::DECIMAL(38,2)]\n",
                "  Join INNER on=[(#0.0::BIGINT = #4.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR]\n",
                "    Aggregate #4 groups=[#1.0::BIGINT] aggregates=[sum(#1.1::DECIMAL(15,2))::DECIMAL(38,2)]\n",
                "      Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
            )
        );
    }

    #[test]
    fn a_count_moves_as_a_sum_of_counts_that_keeps_its_type() {
        let counted = JOINED.replace(
            "sum(#1.1::DECIMAL(15,2))::DECIMAL(38,2)",
            "count(#1.1::DECIMAL(15,2))::BIGINT, count_star()::BIGINT",
        );
        assert_eq!(
            pushed(&counted),
            concat!(
                "Aggregate #3 groups=[#0.1::VARCHAR] aggregates=[sum(#4.1::BIGINT)::BIGINT, sum(#4.2::BIGINT)::BIGINT]\n",
                "  Join INNER on=[(#0.0::BIGINT = #4.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR]\n",
                "    Aggregate #4 groups=[#1.0::BIGINT] aggregates=[count(#1.1::DECIMAL(15,2))::BIGINT, count_star()::BIGINT]\n",
                "      Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
            )
        );
    }

    /// A `count(*)` alone reads no column, so nothing says which side it counts.
    #[test]
    fn a_lone_count_star_or_a_grouping_on_numbers_alone_is_left_where_it_was() {
        let star =
            JOINED.replace("sum(#1.1::DECIMAL(15,2))::DECIMAL(38,2)", "count_star()::BIGINT");
        assert_eq!(pushed(&star), star);
        let numbers = JOINED.replace("groups=[#0.1::VARCHAR]", "groups=[#0.0::BIGINT]");
        assert_eq!(pushed(&numbers), numbers);
    }

    /// TPC-H q13's shape. A customer with no orders comes out of the join with a null where its
    /// count would be, and that customer's count was 0 and its `count(*)` was 1.
    const PADDED: &str = concat!(
        "Aggregate #3 groups=[#0.0::BIGINT] aggregates=[count(#1.1::BIGINT)::BIGINT, count_star()::BIGINT]\n",
        "  Join LEFT on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
        "    Get memory.main.c AS c #0 [k::BIGINT]\n",
        "    Filter (#1.1::BIGINT > 5::BIGINT)::BOOLEAN\n",
        "      Get memory.main.o AS o #1 [c::BIGINT, key::BIGINT]\n",
    );

    /// `PADDED` with the scan of `o` measured at `rows` rows and `distinct` values of `c`.
    fn padded(rows: u64, distinct: u64) -> Plan {
        let mut plan = Plan::parse(PADDED).expect("parses");
        plan.measure(1, Stat::exact(rows, Provenance::RowCount));
        plan.measure_distinct(1, "c", Stat::exact(distinct, Provenance::Dictionary));
        plan
    }

    #[test]
    fn a_count_below_the_padded_side_of_a_left_join_reads_coalesce_above_it() {
        let mut plan = padded(1_500_000, 100_000);
        push(&mut plan);
        plan.validate().expect("valid");
        assert_eq!(
            plan.to_string(),
            concat!(
                "Aggregate #3 groups=[#0.0::BIGINT] aggregates=[sum(coalesce(#4.1::BIGINT, 0::BIGINT)::BIGINT)::BIGINT, sum(coalesce(#4.2::BIGINT, 1::BIGINT)::BIGINT)::BIGINT]\n",
                "  Join LEFT on=[(#0.0::BIGINT = #4.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.c AS c #0 [k::BIGINT]\n",
                "    Aggregate #4 groups=[#1.0::BIGINT] aggregates=[count(#1.1::BIGINT)::BIGINT, count_star()::BIGINT]\n",
                "      Filter (#1.1::BIGINT > 5::BIGINT)::BOOLEAN\n",
                "        Get memory.main.o AS o #1 [c::BIGINT, key::BIGINT]\n",
            )
        );
        let once = plan.to_string();
        push(&mut plan);
        assert_eq!(plan.to_string(), once, "a second run moved the plan again");
    }

    /// Without a string to group by, what decides is how far grouping shrinks B, and that is only
    /// known where the file counted.
    #[test]
    fn a_grouping_on_numbers_moves_only_where_the_file_says_it_shrinks_by_four() {
        for (rows, distinct, moves) in
            [(1_500_000, 100_000, true), (400, 100, true), (400, 101, false)]
        {
            let mut plan = padded(rows, distinct);
            push(&mut plan);
            assert_eq!(plan.to_string() != PADDED, moves, "{rows} rows over {distinct} values");
        }
        assert_eq!(pushed(PADDED), PADDED, "nothing measured");
    }

    #[test]
    fn a_side_whose_kept_columns_are_strings_is_passed_over_for_the_one_below_it() {
        let text = concat!(
            "Aggregate #4 groups=[#0.1::VARCHAR, #2.1::VARCHAR] aggregates=[max(#1.1::DECIMAL(15,2))::DECIMAL(15,2)]\n",
            "  Join INNER on=[(#0.2::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "    Get memory.main.n AS n #2 [k::INTEGER, name::VARCHAR]\n",
            "    Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "      Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR, n::INTEGER]\n",
            "      Filter (#1.1::DECIMAL(15,2) > 1.00::DECIMAL(15,2))::BOOLEAN\n",
            "        Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
        );
        assert_eq!(
            pushed(text),
            concat!(
                "Aggregate #4 groups=[#0.1::VARCHAR, #2.1::VARCHAR] aggregates=[max(#5.1::DECIMAL(15,2))::DECIMAL(15,2)]\n",
                "  Join INNER on=[(#0.2::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
                "    Get memory.main.n AS n #2 [k::INTEGER, name::VARCHAR]\n",
                "    Join INNER on=[(#0.0::BIGINT = #5.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR, n::INTEGER]\n",
                "      Aggregate #5 groups=[#1.0::BIGINT] aggregates=[max(#1.1::DECIMAL(15,2))::DECIMAL(15,2)]\n",
                "        Filter (#1.1::DECIMAL(15,2) > 1.00::DECIMAL(15,2))::BOOLEAN\n",
                "          Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
            )
        );
    }

    #[test]
    fn a_join_that_throws_rows_away_keeps_the_sum_above_it() {
        let filtered = JOINED.replace(
            "    Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR]\n",
            "    Filter (#0.0::BIGINT > 5::BIGINT)::BOOLEAN\n      Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR]\n",
        );
        assert_eq!(pushed(&filtered), filtered);
    }
}
