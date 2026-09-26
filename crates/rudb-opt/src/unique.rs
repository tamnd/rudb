//! A grouping on a column that holds no value twice is a projection.
//!
//! ClickBench 32 and 33 group on `WatchID, ClientIP` and `WatchID` is a different number on every
//! row of `hits`. So every group is one row, `COUNT(*)` is 1 for all of them, `SUM(IsRefresh)` is
//! the row's own `IsRefresh` and `AVG(ResolutionWidth)` is its own `ResolutionWidth`. The operator
//! builds a hash table with a million entries in it to find that out, and the file already knew,
//! because it counted the distinct values of `WatchID` when it was written and the count is the
//! table's row count.
//!
//! So the aggregate becomes a projection over its own input, keeping the aggregate's table index
//! and its output order, group expressions then calls, so nothing above it moves.
//!
//! | call | becomes |
//! |---|---|
//! | `count_star()` | `1` |
//! | `min(e)` | `e` |
//! | `max(e)` | `e` |
//! | `sum(e)` | `e` |
//! | `avg(e)` | `e` |
//!
//! Each of them cast to the type the call returned, so `SUM` of a `SMALLINT` is still a `HUGEINT`.
//! A sum of one value is the value and a null is still null, the empty sum the accumulator returns.
//! An average of one integer is its exact `i128` total over a count of 1, which is the same `DOUBLE`
//! a cast gives. An average over a `DECIMAL` is refused, because the kernel divides the unscaled
//! total and then the scale, and a cast is not promised to round the same way.
//!
//! # When it is the same answer
//!
//! The key has to be a bare column of a scan with only filters, projections that pass the column on
//! and the joins in the next section between the scan and the aggregate, because a filter keeps a
//! subset of the rows and a subset of distinct values is still distinct. A projection that computes
//! the column could make it not distinct at all, so it stops the walk. The counts have to be exact
//! and equal: the table's rows and the column's distinct values, both out of [`Facts`], where only a
//! counted number is kept. And the column has to hold no nulls, declared or counted, since every
//! null row lands in the one null group.
//!
//! `DISTINCT`, a `FILTER` on a call, `count(e)` and any other aggregate are refused. None of them is
//! what a measured query does with this shape.
//!
//! # Through a join
//!
//! TPC-H q10 groups the join of `customer`, `nation` and a total per `o_custkey` on `c_custkey` and
//! six other columns of `customer`. The total is one row per `o_custkey`, and `nation` is one row per
//! `n_nationkey`, so each `customer` row meets at most one row of each and comes out of both joins
//! at most once. `c_custkey` holds every value once in `customer`, so it holds every value once in
//! what the joins produce too, and the aggregate over them hashed seven keys, five of them strings,
//! to put 37,967 rows into 37,967 groups. That was 28 of the 141 ms the query took on one thread.
//!
//! So the walk goes through a join when the key comes from a side whose rows each come out at most
//! once. For an inner or a left join that is the other side matching each of them at most once,
//! which one equality in the condition against a column the other side holds no value twice in
//! says. A semi, an anti, a mark or a single join hands each left row up once whatever it finds.
//! The other side's column is asked the same question the key is, so it can be a column of a scan
//! or the key of an aggregate with one key, which is what the total per `o_custkey` is.
//!
//! The total only exists once eager aggregation has pushed it under the join, which is well after
//! the place in the sequence where this first runs, so [`JoinedRowsAreGroups`] is the same rewrite
//! run again after it.

use rudb_common::{Class, LogicalType, Result, Stat, Value};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan, Slice};

use crate::estimate::{self, Facts, Key};
use crate::fromkey::cast;
use crate::pass::{Context, Pass};
use crate::walk;

/// Turns an aggregate whose groups are its rows into a projection.
#[derive(Debug, Clone, Copy)]
pub struct RowsAreGroups;

impl Pass for RowsAreGroups {
    fn name(&self) -> &'static str {
        "rows_are_groups"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        project_all(plan, context.facts());
        Ok(())
    }
}

/// The same, run again once eager aggregation has put its totals under the joins.
#[derive(Debug, Clone, Copy)]
pub struct JoinedRowsAreGroups;

impl Pass for JoinedRowsAreGroups {
    fn name(&self) -> &'static str {
        "joined_rows_are_groups"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        project_all(plan, context.facts());
        Ok(())
    }
}

/// Rewrites every aggregate in `plan` that groups on a column with no value in it twice.
///
/// What it produces is a projection, which this never looks at, so a second run leaves it alone.
pub fn project_all(plan: &mut Plan, stats: &Facts) {
    let mut moved = false;
    let root =
        walk::restack(plan, plan.root(), &mut moved, &mut |plan, at| project(plan, at, stats));
    if moved {
        plan.set_root(root);
    }
}

/// The projection `at` is, when it is an aggregate whose every group is one row.
fn project(plan: &mut Plan, at: NodeRef, stats: &Facts) -> Option<NodeRef> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else { return None };
    let keys = plan.expr_list(groups).to_vec();
    let calls = plan.expr_list(aggregates).to_vec();
    if !keys.iter().any(|&key| distinct(plan, input, key, stats)) {
        return None;
    }
    let mut outputs = keys;
    for &call in &calls {
        outputs.push(one_row(plan, call)?);
    }
    let names: Vec<_> =
        (0..outputs.len()).map(|position| plan.intern(&format!("column{position}"))).collect();
    let exprs = plan.add_expr_list(&outputs);
    let names = plan.add_name_list(&names);
    Some(plan.add_node(Node::Project { input, index, exprs, names }))
}

/// Whether `key` is a column under `input` that no two rows hold the same value of.
fn distinct(plan: &Plan, input: NodeRef, key: ExprRef, stats: &Facts) -> bool {
    let Expr::Column(outer) = *plan.expr(key) else { return false };
    unique(plan, input, outer, stats)
}

/// Whether no two rows of `at` hold the same value of `binding`, a null counting as a value.
///
/// A filter keeps a subset of the rows and a projection that only passes a column on keeps every row
/// and every value in it, so a column no row holds twice is still one after either. That is the
/// shape a view puts over a file: ClickBench over Parquet reads `hits` through `SELECT * REPLACE
/// (...)`, which rewrites four time columns and passes `WatchID` on untouched. The key of an
/// aggregate with one key is one row per value by construction, a null included. Joins are in the
/// module comment.
fn unique(plan: &Plan, at: NodeRef, binding: ColumnBinding, stats: &Facts) -> bool {
    match *plan.node(at) {
        Node::Get { index, .. } if index == binding.table => counted(plan, at, binding, stats),
        Node::Filter { input, .. } => unique(plan, input, binding, stats),
        Node::Project { input, index, exprs, .. } if index == binding.table => {
            let Some(&expr) = plan.expr_list(exprs).get(binding.column as usize) else {
                return false;
            };
            let Expr::Column(inner) = *plan.expr(expr) else { return false };
            unique(plan, input, inner, stats)
        }
        Node::Aggregate { index, groups, .. } if index == binding.table => {
            binding.column == 0 && plan.expr_list(groups).len() == 1
        }
        Node::Join { left, right, kind, conditions, .. } => {
            let from_left = produces(plan, left, binding);
            match kind {
                JoinKind::Semi | JoinKind::Anti | JoinKind::Mark | JoinKind::Single => {
                    from_left && unique(plan, left, binding, stats)
                }
                JoinKind::Inner | JoinKind::Left => {
                    let (mine, other) = if from_left { (left, right) } else { (right, left) };
                    if !from_left && kind == JoinKind::Left {
                        return false;
                    }
                    (from_left || produces(plan, right, binding))
                        && unique(plan, mine, binding, stats)
                        && once(plan, conditions, other, stats)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether the scan `at` holds every value of `binding` once and no null, by exact counts.
fn counted(plan: &Plan, at: NodeRef, binding: ColumnBinding, stats: &Facts) -> bool {
    let Node::Get { catalog, schema, table, columns, .. } = *plan.node(at) else {
        return false;
    };
    let Some(field) = plan.field_list(columns).get(binding.column as usize) else {
        return false;
    };
    let (catalog, schema, table) = (plan.string(catalog), plan.string(schema), plan.string(table));
    let exact = |stat: Stat<u64>| match stat {
        Stat::Known { value, class: Class::Exact, .. } => Some(value),
        _ => None,
    };
    let rows = exact(stats.get(&Key::Rows { catalog, schema, table }));
    let values = exact(stats.get(&Key::Distinct { catalog, schema, table, column: &field.name }));
    let never_null = field.not_null || estimate::never_null(plan, at, binding);
    never_null && rows.is_some() && rows == values
}

/// Whether `binding` is one of the columns `at` hands up.
fn produces(plan: &Plan, at: NodeRef, binding: ColumnBinding) -> bool {
    walk::outputs(plan, at)
        .is_some_and(|columns| columns.iter().any(|(bound, _)| *bound == binding))
}

/// Whether a join on `conditions` matches each row of the side facing `other` to at most one row
/// of `other`.
///
/// One equality in the `AND` against a column of `other` that no two of its rows share is enough,
/// whatever the rest of the condition says, since the rest can only take matches away. An integer
/// cast over that column is allowed, because one that succeeds maps two values to two values.
fn once(plan: &Plan, conditions: Slice, other: NodeRef, stats: &Facts) -> bool {
    let column = |expr: ExprRef| match *plan.expr(expr) {
        Expr::Column(binding) => Some(binding),
        Expr::Cast { input, try_cast: false }
            if plan.expr_type(expr).is_integer() && plan.expr_type(input).is_integer() =>
        {
            match *plan.expr(input) {
                Expr::Column(binding) => Some(binding),
                _ => None,
            }
        }
        _ => None,
    };
    plan.expr_list(conditions).iter().any(|&condition| {
        let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
            return false;
        };
        [left, right]
            .into_iter()
            .filter_map(column)
            .any(|binding| produces(plan, other, binding) && unique(plan, other, binding, stats))
    })
}

/// What `call` comes to over a group of one row, written against the aggregate's input.
fn one_row(plan: &mut Plan, call: ExprRef) -> Option<ExprRef> {
    let Expr::Aggregate { name, args, distinct, filter } = *plan.expr(call) else { return None };
    if distinct || filter.is_some() {
        return None;
    }
    let want = plan.expr_type(call).clone();
    let span = plan.expr_span(call);
    let written = match (plan.string(name), plan.expr_list(args)) {
        ("count_star", []) => plan.add_constant(Value::BigInt(1)),
        ("min" | "max", &[argument]) => argument,
        ("sum", &[argument]) if plan.expr_type(argument).is_numeric() => argument,
        ("avg", &[argument])
            if plan.expr_type(argument).is_integer()
                || matches!(plan.expr_type(argument), LogicalType::Float | LogicalType::Double) =>
        {
            argument
        }
        _ => return None,
    };
    if !walk::elementwise(plan, written) {
        return None;
    }
    Some(cast(plan, written, &want, span))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::Stat;
    use rudb_common::bounds::{Bound, End, Spread, Test, Zones};
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use super::project_all;
    use crate::estimate::Facts;

    /// A store of one column of that name that says this much about how many nulls it holds.
    #[derive(Debug)]
    struct Stub(&'static str, Stat<u64>);

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            (name == self.0).then_some(0)
        }

        fn surviving(&self, _tests: &[Test]) -> Option<u64> {
            None
        }

        fn spread(&self, _tests: &[Test]) -> Option<Spread> {
            None
        }

        fn extreme(&self, _column: usize, _end: End) -> Stat<Bound> {
            Stat::Unknown
        }

        fn nulls(&self, _column: usize) -> Stat<u64> {
            self.1
        }
    }

    /// ClickBench 32's shape: a filter over the scan and three calls over a two column key.
    const WATCHED: &str = concat!(
        "Aggregate #1 groups=[#0.0::BIGINT, #0.1::INTEGER] aggregates=[count_star()::BIGINT, ",
        "sum(#0.2::SMALLINT)::HUGEINT, avg(#0.3::SMALLINT)::DOUBLE]\n",
        "  Filter (#0.4::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
        "    Get memory.main.hits AS hits #0 [w::BIGINT, ip::INTEGER, r::SMALLINT, x::SMALLINT, ",
        "p::VARCHAR]\n",
    );

    /// `hits` with `rows` rows and `distinct` values of `w`.
    fn counted(rows: u64, distinct: u64) -> Facts {
        let mut facts = Facts::new();
        facts.record("memory", "main", "hits", rows);
        facts.record_distinct("memory", "main", "hits", "w", distinct, Provenance::Dictionary);
        facts
    }

    /// What `text` prints once the pass has run over it twice, with `w` holding `nulls` nulls.
    fn projected(text: &str, stats: &Facts, nulls: u64) -> String {
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let zones = Arc::new(Stub("w", Stat::exact(nulls, Provenance::NullCount)));
        plan.set_zones(0, zones as Arc<dyn Zones>);
        project_all(&mut plan, stats);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        let once = plan.to_string();
        project_all(&mut plan, stats);
        assert_eq!(plan.to_string(), once, "a second run moved the plan again");
        once
    }

    #[test]
    fn a_key_with_a_value_per_row_makes_the_aggregate_a_projection() {
        assert_eq!(
            projected(WATCHED, &counted(1_000, 1_000), 0),
            concat!(
                "Project #1 [#0.0::BIGINT AS column0, #0.1::INTEGER AS column1, 1::BIGINT AS ",
                "column2, CAST(#0.2::SMALLINT)::HUGEINT AS column3, ",
                "CAST(#0.3::SMALLINT)::DOUBLE AS column4]\n",
                "  Filter (#0.4::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
                "    Get memory.main.hits AS hits #0 [w::BIGINT, ip::INTEGER, r::SMALLINT, ",
                "x::SMALLINT, p::VARCHAR]\n",
            )
        );
    }

    /// A projection that passes the key on, as a view over a file does, is walked through, and one
    /// that computes it is not.
    #[test]
    fn a_key_a_projection_passes_on_is_still_a_key() {
        let viewed = concat!(
            "Aggregate #2 groups=[#1.0::BIGINT, #1.1::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "  Project #1 [#0.0::BIGINT AS w, \"+\"(#0.1::INTEGER, 1::INTEGER)::INTEGER AS ip]\n",
            "    Get memory.main.hits AS hits #0 [w::BIGINT, ip::INTEGER, r::SMALLINT, x::SMALLINT, ",
            "p::VARCHAR]\n",
        );
        assert_eq!(
            projected(viewed, &counted(1_000, 1_000), 0),
            concat!(
                "Project #2 [#1.0::BIGINT AS column0, #1.1::INTEGER AS column1, 1::BIGINT AS ",
                "column2]\n",
                "  Project #1 [#0.0::BIGINT AS w, \"+\"(#0.1::INTEGER, 1::INTEGER)::INTEGER AS ip]\n",
                "    Get memory.main.hits AS hits #0 [w::BIGINT, ip::INTEGER, r::SMALLINT, ",
                "x::SMALLINT, p::VARCHAR]\n",
            )
        );
        let computed =
            viewed.replace("#0.0::BIGINT AS w", "\"+\"(#0.0::BIGINT, 1::BIGINT)::BIGINT AS w");
        assert_eq!(projected(&computed, &counted(1_000, 1_000), 0), computed);
    }

    /// One repeated value, one null, or a count nobody took, and a group may hold two rows.
    #[test]
    fn a_key_that_may_repeat_keeps_its_aggregate() {
        assert_eq!(projected(WATCHED, &counted(1_000, 999), 0), WATCHED);
        assert_eq!(projected(WATCHED, &counted(1_000, 1_000), 1), WATCHED);
        let mut rows_only = Facts::new();
        rows_only.record("memory", "main", "hits", 1_000);
        assert_eq!(projected(WATCHED, &rows_only, 0), WATCHED);
    }

    /// The distinct column has to be a key on its own, and every call has to be one this writes.
    #[test]
    fn a_key_it_does_not_hold_or_a_call_it_cannot_write_keeps_its_aggregate() {
        let other = WATCHED.replace("groups=[#0.0::BIGINT, ", "groups=[");
        assert_eq!(projected(&other, &counted(1_000, 1_000), 0), other);
        let counts = WATCHED.replace("count_star()::BIGINT", "count(#0.2::SMALLINT)::BIGINT");
        assert_eq!(projected(&counts, &counted(1_000, 1_000), 0), counts);
    }

    /// TPC-H q10's shape once eager aggregation has run: a total per customer joined back to the
    /// customers and then to their nations, grouped on the customer key and columns beside it.
    const JOINED: &str = concat!(
        "Aggregate #5 groups=[#0.0::BIGINT, #0.1::VARCHAR, #2.1::VARCHAR] ",
        "aggregates=[sum(#4.1::DECIMAL(38,2))::DECIMAL(38,2)]\n",
        "  Join INNER on=[(#0.2::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
        "    Get memory.main.n AS n #2 [k::INTEGER, name::VARCHAR]\n",
        "    Join INNER on=[(#0.0::BIGINT = #4.0::BIGINT)::BOOLEAN]\n",
        "      Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR, n::INTEGER]\n",
        "      Aggregate #4 groups=[#1.0::BIGINT] aggregates=[sum(#1.1::DECIMAL(15,2))::DECIMAL(38,2)]\n",
        "        Get memory.main.o AS o #1 [c::BIGINT, price::DECIMAL(15,2)]\n",
    );

    /// `c` and `n` counted with `customers` and `nations` distinct keys of 100 and 25 rows, and no
    /// null in either key.
    fn joined(text: &str, customers: u64, nations: u64) -> String {
        let mut facts = Facts::new();
        facts.record("memory", "main", "c", 100);
        facts.record_distinct("memory", "main", "c", "k", customers, Provenance::Dictionary);
        facts.record("memory", "main", "n", 25);
        facts.record_distinct("memory", "main", "n", "k", nations, Provenance::Dictionary);
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        for index in [0, 2] {
            let zones = Arc::new(Stub("k", Stat::exact(0, Provenance::NullCount)));
            plan.set_zones(index, zones as Arc<dyn Zones>);
        }
        project_all(&mut plan, &facts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    #[test]
    fn a_key_each_join_hands_up_once_makes_the_aggregate_over_them_a_projection() {
        let out = joined(JOINED, 100, 25);
        assert!(out.starts_with("Project #5 [#0.0::BIGINT AS column0, "), "{out}");
        assert!(!out.contains("Aggregate #5"), "{out}");
        assert!(out.contains("Aggregate #4"), "{out}");
    }

    /// A key that repeats on either side of the join, a total over more than one key, and a key
    /// from the side a left join pads all keep the aggregate.
    #[test]
    fn a_join_that_may_hand_a_row_up_twice_keeps_its_aggregate() {
        assert_eq!(joined(JOINED, 99, 25), JOINED);
        assert_eq!(joined(JOINED, 100, 24), JOINED);
        let wide =
            JOINED.replace("groups=[#1.0::BIGINT]", "groups=[#1.0::BIGINT, #1.1::DECIMAL(15,2)]");
        assert_eq!(joined(&wide, 100, 25), wide);
        let padded = concat!(
            "Aggregate #3 groups=[#2.0::INTEGER] aggregates=[count_star()::BIGINT]\n",
            "  Join LEFT on=[(#0.2::INTEGER = #2.0::INTEGER)::BOOLEAN]\n",
            "    Get memory.main.c AS c #0 [k::BIGINT, name::VARCHAR, n::INTEGER]\n",
            "    Get memory.main.n AS n #2 [k::INTEGER, name::VARCHAR]\n",
        );
        assert_eq!(joined(padded, 100, 25), padded);
    }
}
