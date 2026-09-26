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
//! The key has to be a bare column of a scan with only filters and projections that pass the column
//! on between the scan and the aggregate, because a filter keeps a subset of the rows and a subset
//! of distinct values is still distinct. A join could repeat a row and a projection that computes
//! the column could make it not distinct at all, so both stop the walk. The counts have to be exact and equal: the table's rows and the column's
//! distinct values, both out of [`Facts`], where only a counted number is kept. And the column has
//! to hold no nulls, declared or counted, since every null row lands in the one null group.
//!
//! `DISTINCT`, a `FILTER` on a call, `count(e)` and any other aggregate are refused. None of them is
//! what a measured query does with this shape.

use rudb_common::{Class, LogicalType, Result, Stat, Value};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};

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

/// Whether `key` is a column of a scan under `input` that holds every value once and no null.
fn distinct(plan: &Plan, input: NodeRef, key: ExprRef, stats: &Facts) -> bool {
    let Expr::Column(outer) = *plan.expr(key) else { return false };
    let Some((scan, binding)) = filtered_scan(plan, input, outer) else { return false };
    let Node::Get { catalog, schema, table, columns, .. } = *plan.node(scan) else {
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
    let never_null = field.not_null || estimate::never_null(plan, input, outer);
    never_null && rows.is_some() && rows == values
}

/// The scan `binding` reads and the column of it that is, when nothing stands between it and `at`
/// but filters and projections that hand the column up as it is.
///
/// A projection that only passes a column on keeps every row and every value in it, so a column no
/// row holds twice is still one after it. That is the shape a view puts over a file: ClickBench over
/// Parquet reads `hits` through `SELECT * REPLACE (...)`, which rewrites four time columns and passes
/// `WatchID` on untouched.
fn filtered_scan(
    plan: &Plan,
    at: NodeRef,
    binding: ColumnBinding,
) -> Option<(NodeRef, ColumnBinding)> {
    match *plan.node(at) {
        Node::Get { index, .. } if index == binding.table => Some((at, binding)),
        Node::Filter { input, .. } => filtered_scan(plan, input, binding),
        Node::Project { input, index, exprs, .. } if index == binding.table => {
            let &expr = plan.expr_list(exprs).get(binding.column as usize)?;
            let Expr::Column(inner) = *plan.expr(expr) else { return None };
            filtered_scan(plan, input, inner)
        }
        _ => None,
    }
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

    /// A store of one column called `w` that says this much about how many nulls it holds.
    #[derive(Debug)]
    struct Stub(Stat<u64>);

    impl Zones for Stub {
        fn column(&self, name: &str) -> Option<usize> {
            (name == "w").then_some(0)
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
            self.0
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
        let zones = Arc::new(Stub(Stat::exact(nulls, Provenance::NullCount)));
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
        let computed = viewed.replace("#0.0::BIGINT AS w", "\"+\"(#0.0::BIGINT, 1::BIGINT)::BIGINT AS w");
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
}
